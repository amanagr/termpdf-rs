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
mod outline;
mod pdf;
mod pdfhighlights;
mod search;
mod session;
mod textlayout;
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
#[command(
    version,
    about = "Terminal PDF reader (kitty/sixel/halfblocks)",
    arg_required_else_help = true,
)]
struct Args {
    /// Path to the PDF file.
    #[arg(value_hint = clap::ValueHint::FilePath)]
    path: PathBuf,
    /// Page to open initially (1-indexed). Default: 1.
    #[arg(short, long, default_value_t = 1)]
    page: usize,
    /// Start in dark mode (luminance-only inversion).
    #[arg(short, long)]
    dark: bool,
    /// Force a specific terminal graphics protocol. `auto` (default)
    /// probes the terminal and falls back to halfblocks. Env-resolved
    /// values are validated by clap (typo → hard error rather than a
    /// silent fall-through to auto, which is what the hand-rolled
    /// parser used to do).
    #[arg(
        long,
        value_enum,
        env = "TERMPDF_PROTOCOL",
        default_value_t = ProtocolChoice::Auto,
    )]
    protocol: ProtocolChoice,
    /// Smoke test: render the requested page once (no TUI, no terminal
    /// query) and write the result PNG to the given path. Useful for CI
    /// or for sanity-checking pdfium + dark inversion without a TTY.
    #[arg(long, value_name = "PNG", value_hint = clap::ValueHint::FilePath)]
    probe: Option<PathBuf>,
    /// Zoom factor for `--probe` only (1.0 = fit; >1 = zoomed pixmap).
    /// Lets the rendering path under zoom be exercised headlessly.
    #[arg(long, default_value_t = 1.0)]
    probe_zoom: f32,
}

/// Build a `Picker` for the resolved protocol. The CLI/env merge is
/// done by clap (`env = "TERMPDF_PROTOCOL"` on the `--protocol` arg),
/// so this function only needs to translate the choice into a Picker.
fn pick_protocol(resolved: ProtocolChoice) -> Picker {
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
/// `allow-passthrough` configured. We can't reliably probe the
/// passthrough setting from here, so we print the hint at most ONCE
/// per machine: after the first run an ack-marker is dropped under
/// `$XDG_DATA_HOME/termpdf-rs/`. Suppressible per-invocation via
/// `$TERMPDF_NO_TMUX_HINT`; the marker itself can be deleted to
/// re-arm the hint.
fn tmux_passthrough_hint() {
    if std::env::var("TMUX").is_err() || std::env::var("TERMPDF_NO_TMUX_HINT").is_ok() {
        return;
    }
    let marker = dirs::data_local_dir().map(|d| d.join("termpdf-rs/.tmux-hint-acked"));
    if let Some(ref m) = marker {
        if m.exists() {
            return;
        }
    }
    eprintln!(
        "note: running inside tmux. If images don't render, run\n  \
         tmux set -g allow-passthrough on\n  \
         (this hint won't repeat — re-arm by deleting \
         $XDG_DATA_HOME/termpdf-rs/.tmux-hint-acked)"
    );
    if let Some(m) = marker {
        if let Some(parent) = m.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&m, b"");
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
    let document = match pdfium.load_pdf_from_file(&args.path, None) {
        Ok(d) => d,
        Err(e) => {
            // Translate pdfium's "PASSWORD required" into a clear,
            // user-actionable message rather than a raw anyhow chain.
            // Password-protected PDFs aren't supported yet — surface
            // the limitation explicitly so the user doesn't think the
            // file is corrupt.
            use pdfium_render::prelude::{PdfiumError, PdfiumInternalError};
            if matches!(
                &e,
                PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError)
            ) {
                anyhow::bail!(
                    "{}: PDF is password-protected; termpdf-rs does not yet \
                     support encrypted documents. Decrypt with `qpdf --decrypt` \
                     or open in another reader.",
                    args.path.display()
                );
            }
            return Err(e).with_context(|| format!("loading PDF: {}", args.path.display()));
        }
    };

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

    let mut app = App::new(
        document,
        &args.path,
        start_page,
        start_dark,
        saved.zoom,
        saved.marks.clone(),
        picker,
    )?;

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

        // Block once for the next event (250 ms tick), then drain the
        // queue without redrawing in between. Two reasons:
        //   1. Mouse-drag fires 30–100 events/sec; a redraw per event
        //      thrashes the kitty graphics pipeline. Coalescing keeps
        //      only the *latest* drag position before the next paint.
        //   2. Buffered keystrokes (user mashing Space) collapse into
        //      one redraw — the user just sees the final scroll position
        //      instead of a slow staircase of full re-encodes.
        if event::poll(Duration::from_millis(250))? {
            dispatch_event(app, event::read()?)?;
            // Drain any other pending events (poll(0) returns immediately).
            // Stop early on quit so we don't keep dispatching after a `:q`.
            while !app.should_quit && event::poll(Duration::ZERO)? {
                dispatch_event(app, event::read()?)?;
            }
        }
    }
    Ok(())
}

fn dispatch_event(app: &mut App<'_>, ev: Event) -> Result<()> {
    match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => keys::dispatch(app, k)?,
        Event::Resize(_, _) => app.invalidate_compose(),
        Event::Mouse(m) => keys::dispatch_mouse(app, m)?,
        _ => {}
    }
    Ok(())
}
