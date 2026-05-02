//! termpdf-rs — terminal PDF reader.
//!
//! Architecture in three layers:
//!   1. `pdf` — wraps pdfium-render. Renders a page index to an
//!      `image::DynamicImage` at a target pixel size.
//!   2. `dark` — optional luminance-only HSL inversion of the image
//!      (so red text doesn't become cyan).
//!   3. `ui` — ratatui frame with `ratatui-image` displaying the
//!      page via Kitty graphics + a thin status bar / `?` overlay.
//!
//! Vim-style modes:
//!   Normal:  j/k page nav, gg/G, N-prefix counts, +/- zoom, d dark
//!   Command: `:23` jump, `:q` quit, `:set [no]dark`
//!   Visual:  v…y to capture a highlight rectangle (stub for v0.1)
//!   Search:  / to enter (stub for v0.1)

mod app;
mod clipboard;
mod cmd;
mod compose;
mod dark;
mod highlight;
mod keys;
mod layout;
mod pdf;
mod search;
mod session;
mod text;
mod ui;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{CommandFactory, FromArgMatches, Parser};
use clap::parser::ValueSource;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use ratatui_image::picker::{Picker, ProtocolType};

use crate::app::App;

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ProtocolChoice {
    /// Probe the terminal; fall back to halfblocks if no answer.
    Auto,
    Kitty,
    Sixel,
    Iterm2,
    /// Always-works fallback. Text is essentially unreadable.
    Halfblocks,
}

#[derive(Parser, Debug)]
#[command(version, about = "Terminal PDF reader (kitty/sixel/halfblocks)")]
struct Args {
    /// Path to the PDF file.
    path: PathBuf,
    /// Page to open initially (1-indexed). Default: 1.
    #[arg(short, long, default_value_t = 1)]
    page: usize,
    /// Start in dark mode (luminance-only inversion).
    #[arg(short, long)]
    dark: bool,
    /// Force a specific terminal graphics protocol. `auto` (default)
    /// probes the terminal and falls back to halfblocks. May also be
    /// set via `$TERMPDF_PROTOCOL`.
    #[arg(long, value_enum, default_value_t = ProtocolChoice::Auto)]
    protocol: ProtocolChoice,
    /// Smoke test: render the requested page once (no TUI, no terminal
    /// query) and write the result PNG to the given path. Useful for CI
    /// or for sanity-checking pdfium + dark inversion without a TTY.
    #[arg(long, value_name = "PNG")]
    probe: Option<PathBuf>,
    /// Zoom factor for `--probe` only (1.0 = fit; >1 = zoomed pixmap).
    /// Lets the rendering path under zoom be exercised headlessly.
    #[arg(long, default_value_t = 1.0)]
    probe_zoom: f32,
}

/// Resolve the effective protocol given the CLI flag and
/// `$TERMPDF_PROTOCOL`, then build a `Picker`. CLI flag wins; env
/// only kicks in when the flag is left at its default `auto`.
fn pick_protocol(cli: ProtocolChoice) -> Picker {
    let resolved = if cli != ProtocolChoice::Auto {
        cli
    } else {
        match std::env::var("TERMPDF_PROTOCOL").ok().as_deref() {
            Some("kitty") => ProtocolChoice::Kitty,
            Some("sixel") => ProtocolChoice::Sixel,
            Some("iterm2") => ProtocolChoice::Iterm2,
            Some("halfblocks") => ProtocolChoice::Halfblocks,
            _ => ProtocolChoice::Auto,
        }
    };

    if resolved == ProtocolChoice::Auto {
        // Probe; degrade to halfblocks when the terminal answers
        // nothing (xterm without sixel, basic Windows Terminal, …).
        match Picker::from_query_stdio() {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "warning: terminal didn't advertise a graphics protocol ({e}). \
                     Falling back to halfblocks — text will be illegible. \
                     Try `--protocol sixel` or run inside Kitty/Ghostty/WezTerm."
                );
                Picker::halfblocks()
            }
        }
    } else {
        // Try probing first to get an accurate font_size; if that
        // fails, halfblocks() supplies a sensible default. Then pin
        // the requested protocol on top.
        let mut picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
        let target = match resolved {
            ProtocolChoice::Kitty => ProtocolType::Kitty,
            ProtocolChoice::Sixel => ProtocolType::Sixel,
            ProtocolChoice::Iterm2 => ProtocolType::Iterm2,
            ProtocolChoice::Halfblocks => ProtocolType::Halfblocks,
            ProtocolChoice::Auto => unreachable!(),
        };
        picker.set_protocol_type(target);
        picker
    }
}

/// One-line stderr hint when running inside tmux without
/// `allow-passthrough` configured. We can't reliably detect the
/// passthrough flag, so we always print the hint when `$TMUX` is set
/// (suppressible via `$TERMPDF_NO_TMUX_HINT`).
fn tmux_passthrough_hint() {
    if std::env::var("TMUX").is_ok() && std::env::var("TERMPDF_NO_TMUX_HINT").is_err() {
        eprintln!(
            "note: running inside tmux. If images don't render, run\n  \
             tmux set -g allow-passthrough on\n  \
             (silence with TERMPDF_NO_TMUX_HINT=1)"
        );
    }
}

fn main() -> Result<()> {
    // Parse via ArgMatches so we can tell whether `--page` / `--dark`
    // came from the CLI (override saved state) or from clap's default
    // (fall back to whatever the user left on last exit).
    let matches = Args::command().get_matches();
    let page_explicit = matches.value_source("page") == Some(ValueSource::CommandLine);
    let dark_explicit = matches.value_source("dark") == Some(ValueSource::CommandLine);
    let args = Args::from_arg_matches(&matches)?;

    let lib = pdf::find_libpdfium()
        .context("locating libpdfium.so — run setup.sh in the project root")?;
    let bindings = pdf::bindings(&lib)?;
    let pdfium = pdfium_render::prelude::Pdfium::new(bindings);
    let document = pdfium
        .load_pdf_from_file(&args.path, None)
        .with_context(|| format!("loading PDF: {}", args.path.display()))?;

    // Restore last page + dark flag for this PDF, but let an explicit
    // CLI value (`-p 12`, `--dark`) win over the saved session.
    let saved = session::Session::load(&args.path);
    let start_page = if page_explicit {
        args.page.saturating_sub(1)
    } else {
        saved.page
    };
    let start_dark = if dark_explicit { args.dark } else { saved.dark };

    if let Some(out) = args.probe {
        return probe(&document, start_page, start_dark, args.probe_zoom, &out);
    }

    tmux_passthrough_hint();
    let picker = pick_protocol(args.protocol);

    let mut app = App::new(document, &args.path, start_page, start_dark, picker)?;

    setup_terminal()?;
    let res = run_loop(&mut app);
    teardown_terminal()?;

    if let Err(e) = app.persist_highlights() {
        eprintln!("warning: failed to persist highlights: {e:?}");
    }
    if let Err(e) = app.persist_session() {
        eprintln!("warning: failed to persist session: {e:?}");
    }
    res
}

fn setup_terminal() -> Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    Ok(())
}

fn teardown_terminal() -> Result<()> {
    execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

/// Headless render path used by `--probe`. Builds a fake Picker with an
/// 8×16 cell size (typical terminal proportions) and renders one page
/// to a PNG. Validates that pdfium loads, the page parses, and (if
/// `dark`) the luminance-inversion pipeline runs without panicking.
fn probe(
    document: &pdfium_render::prelude::PdfDocument<'_>,
    page: usize,
    dark: bool,
    zoom: f32,
    out: &std::path::Path,
) -> Result<()> {
    use ratatui::layout::Rect;
    let picker = Picker::halfblocks();
    let area = Rect { x: 0, y: 0, width: 80, height: 40 };
    let (cell_w, _cell_h) = picker.font_size();
    let target_w = (((area.width as u32) * (cell_w as u32)) as f32 * zoom) as u32;
    let img = pdf::render_page_at_width(document, page, target_w.max(1))?;
    let img = if dark {
        image::DynamicImage::ImageRgba8(dark::invert_luminance(&img))
    } else {
        img
    };
    img.save(out)
        .with_context(|| format!("writing probe PNG to {}", out.display()))?;
    eprintln!(
        "probe: wrote {}×{} PNG to {} (page {}, dark={})",
        img.width(),
        img.height(),
        out.display(),
        page + 1,
        dark
    );
    Ok(())
}

fn run_loop(app: &mut App<'_>) -> Result<()> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut term = Terminal::new(backend)?;

    while !app.should_quit {
        term.draw(|f| ui::draw(f, app))?;

        // 250 ms tick so a future "auto-reload on file change" or an
        // animated cursor in command mode just slot into the same loop.
        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => keys::dispatch(app, k)?,
                Event::Resize(_, _) => app.invalidate_compose(),
                Event::Mouse(m) => keys::dispatch_mouse(app, m)?,
                _ => {}
            }
        }
    }
    Ok(())
}
