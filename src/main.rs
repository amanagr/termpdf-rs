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
mod cmd;
mod compose;
mod dark;
mod highlight;
mod keys;
mod pdf;
mod session;
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
use ratatui_image::picker::Picker;

use crate::app::App;

#[derive(Parser, Debug)]
#[command(version, about = "Terminal PDF reader (Kitty graphics + dark mode)")]
struct Args {
    /// Path to the PDF file.
    path: PathBuf,
    /// Page to open initially (1-indexed). Default: 1.
    #[arg(short, long, default_value_t = 1)]
    page: usize,
    /// Start in dark mode (luminance-only inversion).
    #[arg(short, long)]
    dark: bool,
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

    // ratatui-image probes the terminal at startup to figure out which
    // graphics protocol it can speak (kitty, sixel, halfblocks). On
    // Ghostty this lands on the Kitty protocol; we don't have to
    // hand-encode the escape sequences ourselves.
    let picker = Picker::from_query_stdio()
        .context("querying terminal for graphics protocol — is this a Kitty/Ghostty/WezTerm?")?;

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
    // `halfblocks()` is the only Picker constructor that doesn't query
    // the terminal. We don't actually render with it — we just need the
    // font_size for pdfium's pixel-target math. (10×20 px per cell.)
    let picker = Picker::halfblocks();
    let area = Rect { x: 0, y: 0, width: 80, height: 40 };
    let img = pdf::render_page(document, page, area, &picker, zoom)?;
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
                Event::Resize(_, _) => app.invalidate(),
                Event::Mouse(m) => keys::dispatch_mouse(app, m)?,
                _ => {}
            }
        }
    }
    Ok(())
}
