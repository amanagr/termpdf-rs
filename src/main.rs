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
mod disk_cache;
mod highlight;
mod keys;
mod kitty_pages;
mod layout;
mod links;
mod outline;
mod pdf;
mod pdfhighlights;
mod profile;
mod render_worker;
mod search;
mod search_index;
mod session;
mod term_safe;
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
        // Explicit `--protocol` means the user already knows what
        // their terminal supports — skip the stdio probe so we work
        // even when nothing on the wire will answer (CI, pty-driven
        // integration tests, captured pipelines).
        //
        // We still need a cell font_size for the layout math, so
        // honour `$TERMPDF_CELL_PX="WxH"` if set; otherwise use a
        // sensible 8×16 default that matches most terminals close
        // enough for headless smoke tests.
        let cell = std::env::var("TERMPDF_CELL_PX")
            .ok()
            .and_then(|s| {
                let mut it = s.split('x');
                let w: u16 = it.next()?.parse().ok()?;
                let h: u16 = it.next()?.parse().ok()?;
                Some((w, h))
            })
            .unwrap_or((8, 16));
        // `from_fontsize` is the only constructor that doesn't probe
        // stdio; the deprecation nudges everyone toward `from_query_stdio`,
        // but for the explicit-protocol case that's exactly what we
        // need to avoid (and the alternative `halfblocks()` would force
        // the wrong protocol type).
        #[allow(deprecated)]
        let mut picker = Picker::from_fontsize(cell);
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
        // Open with O_NOFOLLOW so a pre-planted symlink at the marker
        // path can't redirect our zero-byte write to a sensitive
        // file (e.g. ~/.bashrc) that the user has write access to.
        // O_CREAT means "create if missing"; the lack of O_EXCL is
        // deliberate — re-writing on every run is fine, the marker
        // only needs to *exist*, not be unique.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            const O_NOFOLLOW: i32 = 0x20000;
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .custom_flags(O_NOFOLLOW)
                .open(&m);
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::write(&m, b"");
        }
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
    // Trim oldest disk-cache entries above the byte budget. Runs in
    // a background thread so the filesystem walk doesn't add to
    // first-paint latency.
    disk_cache::evict_to_budget_async();
    let picker = pick_protocol(args.protocol);

    // If the user passed `--page` on the CLI, they meant "open at this
    // page (top)" — discard the saved within-page scroll. Otherwise
    // restore the exact pixel position they were at last session.
    let start_scroll_in_page = if page_explicit { 0.0 } else { saved.scroll_in_page };
    let mut app = App::new(
        document,
        &args.path,
        start_page,
        start_dark,
        saved.zoom,
        saved.marks.clone(),
        start_scroll_in_page,
        picker,
    )?;

    // Background render worker for prefetch (steady-scroll smoothness).
    // Failure to spawn falls back to fully synchronous rendering — the
    // user sees the same UX as before this commit, just without the
    // prefetch speedup.
    app.render_worker = render_worker::RenderWorker::spawn(lib.clone(), args.path.clone());

    setup_terminal()?;
    install_decset_panic_hook();
    let res = run_loop(&mut app);
    teardown_terminal()?;
    profile::report();

    if let Err(e) = app.persist_highlights() {
        eprintln!(
            "warning: failed to persist highlights: {}",
            term_safe::safe_for_stderr(&format!("{e:?}"))
        );
    }
    if let Err(e) = app.persist_session() {
        eprintln!(
            "warning: failed to persist session: {}",
            term_safe::safe_for_stderr(&format!("{e:?}"))
        );
    }
    res
}

fn setup_terminal() -> Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    Ok(())
}

fn teardown_terminal() -> Result<()> {
    // Belt-and-braces ESU: if a frame ended in a state where the
    // run-loop's per-frame ESU didn't run (e.g. an early return path
    // was added later that bypassed it), make sure we're not leaving
    // the user's terminal in DECSET 2026 sync mode.
    use std::io::Write as _;
    let mut out = io::stdout();
    let _ = write!(out, "\x1b[?2026l");
    let _ = out.flush();
    execute!(out, DisableMouseCapture, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(())
}

/// Wrap the default panic hook so a panic during `term.draw` or any
/// other tight render path doesn't leave the user's terminal stuck
/// in DECSET 2026 sync mode (output buffered, screen frozen). Writes
/// ESU directly to stdout — bypasses any BufWriter that might still
/// hold the BSU we paired it with, since by panic time the unwinder
/// hasn't run our normal cleanup. Also clears alt-screen + raw mode
/// so the user's shell prompt is reachable.
fn install_decset_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write as _;
        let mut out = io::stdout();
        let _ = write!(out, "\x1b[?2026l");
        let _ = out.flush();
        // Best-effort tty restore; if we can't (e.g. stdout already
        // closed) the default hook still runs and the user can fix
        // the terminal with `reset`.
        let _ = execute!(out, DisableMouseCapture, LeaveAlternateScreen);
        let _ = disable_raw_mode();
        prev(info);
    }));
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
        image::DynamicImage::ImageRgba8(dark::invert_luminance(img))
    } else {
        img
    };
    img.save(out)
        .with_context(|| format!("writing probe PNG to {}", out.display()))?;
    eprintln!(
        "probe: wrote {}×{} PNG to {} (page {}, dark={})",
        img.width(),
        img.height(),
        term_safe::safe_for_stderr(&out.display().to_string()),
        page + 1,
        dark
    );
    Ok(())
}

fn run_loop(app: &mut App<'_>) -> Result<()> {
    // BufWriter wrap of stdout — without this, every cell ratatui's
    // diff emits goes through a separate `write_all` syscall on the
    // raw stdout (which acquires its own mutex per call). For a
    // multi-MB kitty image transmit chunked into ~85 base64 blocks,
    // that's 85+ syscalls per frame; with the BufWriter all the
    // bytes accumulate in a 256 KiB user-space buffer and drain in
    // one syscall on `term.draw()`'s explicit flush at end-of-frame.
    // Two perf-research agents independently flagged this as the
    // single biggest easy win; observed 5–10× syscall reduction on
    // image-heavy frames.
    //
    // Idle-warm prefetch (further down) writes to stdout directly
    // (bypassing this BufWriter) — that's fine because it runs
    // strictly between draws when ratatui's flush has already
    // drained, so the byte order on the pty stays consistent.
    let backend =
        CrosstermBackend::new(std::io::BufWriter::with_capacity(256 * 1024, io::stdout()));
    let mut term = Terminal::new(backend)?;
    // Watchdog: minimum interval between actual paints under sustained
    // input. 16 ms ≈ 60 Hz. Lets a held-`j` collapse multiple steps
    // into one paint while keeping single keys feeling responsive.
    const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(16);
    /// How long after the last input we wait before firing the
    /// catch-up draw that does the deferred cold-page transmits.
    /// Quick enough that letting up a held key feels instant; long
    /// enough that brief pauses during normal-cadence reading don't
    /// trigger an expensive catch-up draw mid-burst.
    const SETTLE_MS: u128 = 120;
    let mut last_draw = std::time::Instant::now() - MIN_FRAME_INTERVAL;

    // After a rapid-input burst settles, we want to schedule one
    // catch-up draw that does the deferred cold-page transmits.
    // Tracked here in the loop rather than per-event so we don't
    // forget if multiple events arrive within the threshold.
    let mut needs_settle_redraw = false;

    // Idle gating: only call term.draw when something has actually
    // changed since the last paint. Without this, the loop poked
    // ratatui every 250 ms (idle poll cadence), each draw causing
    // Ghostty to wake on the pty and re-composite — sustained 50-90 %
    // Ghostty CPU even with the PDF "doing nothing." First frame
    // always paints; subsequent frames require an explicit dirty
    // signal: input dispatched, settle catch-up, cold-redraw catch-up.
    let mut dirty = true;

    while !app.should_quit {
        // Pre-draw peek: if more input is already queued, don't paint
        // the intermediate state — drain first, then paint the result.
        // The watchdog guarantees we still paint at ~60 Hz so very
        // long input bursts (mouse drags, autorepeat) don't lock the
        // screen.
        let mut should_draw = dirty;
        let now = std::time::Instant::now();
        if now.duration_since(last_draw) < MIN_FRAME_INTERVAL && event::poll(Duration::ZERO)? {
            should_draw = false;
        }
        if should_draw {
            let _draw = profile::span(profile::Phase::Draw);
            // Wrap the frame in DECSET 2026 (Synchronized Output): the
            // terminal buffers everything between BSU (\x1b[?2026h) and
            // ESU (\x1b[?2026l) and commits it as one atomic frame.
            // Supported by Ghostty, Kitty, Wezterm, recent xterm;
            // unsupported terminals silently ignore the unknown
            // private mode.
            //
            // CRITICAL: we MUST write ESU on every exit path. If BSU
            // hits the terminal but ESU does not (term.draw error,
            // panic mid-frame, write failure), the terminal stays
            // stuck in synchronized-output mode — output buffered,
            // never committed, screen frozen even after the user
            // stops scrolling. So we capture the draw result, ALWAYS
            // emit ESU + flush, then propagate the error after.
            // Panics inside ui::draw are caught by the panic hook
            // installed in main(), which writes ESU directly to
            // stdout before the default handler runs.
            use std::io::Write as _;
            let _ = write!(term.backend_mut(), "\x1b[?2026h");
            // Discard CompletedFrame's borrow on term immediately —
            // we need term back to write ESU. Result -> Result<(),_>.
            let draw_res = term.draw(|f| ui::draw(f, app)).map(|_| ());
            let _ = write!(term.backend_mut(), "\x1b[?2026l");
            let _ = term.backend_mut().flush();
            draw_res?;
            drop(_draw);
            last_draw = std::time::Instant::now();
            dirty = false;
        }

        // Settle-redraw poll timing: when the user is mid-burst we
        // don't want to wait 250 ms before noticing the burst ended.
        // Drop the poll wait to ~SETTLE_MS so the catch-up draw fires
        // promptly once input goes idle.
        let poll_dur = if needs_settle_redraw {
            Duration::from_millis(SETTLE_MS as u64)
        } else {
            Duration::from_millis(250)
        };

        // Block once for the next event (poll_dur tick) then drain
        // the queue. Two reasons to drain in batches:
        //   1. Mouse-drag fires 30–100 events/sec; one redraw per
        //      event thrashes the kitty graphics pipeline.
        //   2. Held-key autorepeat (j/Space/l) collapses into one
        //      redraw at the final state.
        // Cold-render staggering: if the just-finished draw deferred
        // any cold-page renders past its budget, jump straight back to
        // the top of the loop with the watchdog allowed to fire so the
        // catch-up renders happen one-per-frame instead of all-at-once.
        // Skipping the event::poll here is what gives the catch-up
        // its ~16 ms cadence — long enough that Ghostty's renderer
        // can drain each transmit before the next arrives.
        if app.pending_cold_redraw {
            app.pending_cold_redraw = false;
            last_draw = std::time::Instant::now() - MIN_FRAME_INTERVAL;
            dirty = true;
            continue;
        }

        if event::poll(poll_dur)? {
            dispatch_event_coalesced(app, event::read()?)?;
            app.note_input();
            needs_settle_redraw = true;
            dirty = true;
            while !app.should_quit && event::poll(Duration::ZERO)? {
                dispatch_event_coalesced(app, event::read()?)?;
                app.note_input();
            }
        } else if needs_settle_redraw {
            // Poll timed out → input is idle for SETTLE_MS. Reset the
            // input burst so is_rapid_scrolling returns false on the
            // catch-up draw — otherwise SETTLE_MS (120) <
            // RAPID_SCROLL_THRESHOLD_MS (250) and the catch-up
            // re-defers the cold page, leaving the current page blank
            // until 130 ms later. The next real scroll re-arms the
            // burst counter from 1, so this only affects the gap
            // between this catch-up and the next input.
            app.clear_input_burst();
            needs_settle_redraw = false;
            last_draw = std::time::Instant::now() - MIN_FRAME_INTERVAL;
            dirty = true;
        } else {
            // True idle: no input pending and no settle to do. Use
            // the moment to warm one upcoming page so the next j/k
            // press hits the cache instead of a 20 ms pdfium render.
            //
            // Idle work only kicks in *after* the user's first real
            // input — opening a fresh 600-page PDF should not
            // immediately spawn a tick storm of pdfium renders + tmux
            // passthrough kitty transmits before the user has done
            // anything. (Previously `unwrap_or(true)` treated "no
            // input yet" as idle, producing ~40% CPU and a flood of
            // kitty graphics escapes through tmux on open.)
            let action = idle_action(app.last_input_at.map(|t| t.elapsed()));
            if action != IdleAction::Skip {
                let _s = profile::span(profile::Phase::IdleWarm);
                let _ = warm_one_idle(app, action);
            }
        }
    }
    Ok(())
}

/// First idle-tier threshold: how long the user must be idle before
/// we run any background work at all.
pub(crate) const IDLE_BITMAP_WARM_MS: u64 = 200;
/// Second idle-tier threshold: doc-text indexing is heavier (a single
/// `page.text().all()` is 5–50 ms on dense pages) and runs every tick
/// for the entire doc on first open. 2000 ms (was 1000 ms) — text
/// indexing is best-effort (search without a complete index just
/// scans live), so deferring it longer is a clean win for a reading
/// session on a big book; `index.bin` still persists once complete
/// for the next open.
pub(crate) const IDLE_TEXT_INDEX_MS: u64 = 2000;
/// Cap on bitmap warms per idle tick. 2 is enough to keep j-press hot
/// (one for the page about to come into view, one as a buffer) while
/// halving the per-tick burst of PNG transmits through tmux that
/// previously locked Ghostty up on big books.
pub(crate) const MAX_WARMS_PER_IDLE: u32 = 2;

/// What kind of idle-tick work the run-loop should consider doing,
/// based on how long the user has been idle since their last input.
/// Pulled out as a pure function so the tier policy is unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdleAction {
    /// User has not yet given any input this session, or the idle
    /// window is too short — skip background work entirely.
    Skip,
    /// Bitmap prefetch only.
    BitmapWarm,
    /// Bitmap prefetch *and* doc-text indexing.
    BitmapWarmAndTextIndex,
}

pub(crate) fn idle_action(last_input_elapsed: Option<Duration>) -> IdleAction {
    let Some(elapsed) = last_input_elapsed else {
        return IdleAction::Skip;
    };
    if elapsed >= Duration::from_millis(IDLE_TEXT_INDEX_MS) {
        IdleAction::BitmapWarmAndTextIndex
    } else if elapsed >= Duration::from_millis(IDLE_BITMAP_WARM_MS) {
        IdleAction::BitmapWarm
    } else {
        IdleAction::Skip
    }
}

/// Idle-time prefetch. Renders + pre-encodes upcoming pages in the
/// user's scroll direction so the next j/k press lands on a cached
/// page instead of a synchronous ~20 ms pdfium call.
///
/// Loops up to `MAX_WARMS_PER_IDLE` times in a single call, polling
/// for input between iterations so a keystroke mid-prefetch breaks
/// out immediately.
///
/// Also opportunistically extends the doc-text index (for fast
/// search) but only after `IDLE_TEXT_INDEX_MS` of idle — the text
/// extract is heavy enough on dense pages that running it every
/// 250 ms tick is a sustained drain on a freshly-opened doc.
fn warm_one_idle(app: &mut App<'_>, action: IdleAction) -> Result<()> {
    let do_text = action == IdleAction::BitmapWarmAndTextIndex;
    for _ in 0..MAX_WARMS_PER_IDLE {
        if !warm_next_uncached(app)? {
            // Bitmap prefetch is full → spend the rest of the idle
            // tick on whichever quality / index work remains. Order:
            //   1. Upgrade one visible page from Fast → Sharp so the
            //      user sees crisp text after a settle.
            //   2. Then text indexing if we're past the deeper idle
            //      threshold.
            // Doing the upgrade FIRST keeps the visible quality
            // converging quickly; text indexing has a longer runway
            // (its results enter via `n` after a `/` query) and is
            // fine to run after.
            let _ = upgrade_one_visible_to_sharp(app);
            if do_text {
                index_one_page_text(app);
            }
            return Ok(());
        }
        if event::poll(Duration::ZERO)? {
            return Ok(());
        }
    }
    if do_text {
        index_one_page_text(app);
    }
    Ok(())
}

/// Re-render at most one visible Fast-quality page at Sharp quality.
/// Returns Ok(true) on a successful upgrade, Ok(false) if no candidate.
/// Caller is the idle path so the ~25-40 ms pdfium hit lands while
/// the user's hands aren't moving.
fn upgrade_one_visible_to_sharp(app: &mut App<'_>) -> Result<bool> {
    let viewport_h = app.viewport_px.1;
    if viewport_h == 0 {
        return Ok(false);
    }
    if app.pages_at_fast_quality.is_empty() {
        return Ok(false);
    }
    let visible: Vec<usize> = app
        .layout
        .visible_pages(app.scroll_y_px, viewport_h)
        .collect();
    let fit_width_px = app.layout.fit_width_px;
    for &pi in &visible {
        if app.pages_at_fast_quality.contains(&pi) {
            return ui::upgrade_page_to_sharp(app, pi, fit_width_px);
        }
    }
    Ok(false)
}

/// Extract text for the next un-indexed page and add to the search
/// index. ~5–50 ms per page depending on density. Caller gates this
/// behind `IDLE_TEXT_INDEX_MS` of elapsed idle so it doesn't run
/// during active reading; bounded to one call per `warm_one_idle`
/// so a keystroke mid-extract costs at most one page of work.
///
/// On the transition from incomplete → complete, persists the index
/// to disk so the next open of the same PDF skips the indexing
/// cost entirely.
fn index_one_page_text(app: &mut App<'_>) {
    if app.doc_index.is_complete() {
        return;
    }
    let Some(page_idx) = app.doc_index.next_page_to_index() else {
        return;
    };
    let pages = app.document.pages();
    let Ok(page) = pages.get(page_idx as i32) else {
        app.doc_index.add_page(page_idx, String::new());
        return;
    };
    let text = match page.text() {
        Ok(t) => t.all(),
        Err(_) => String::new(),
    };
    app.doc_index.add_page(page_idx, text);

    // First time the index becomes complete, snapshot it to disk.
    // Failure is silent (cache is best-effort).
    if app.doc_index.is_complete() && !app.index_persisted {
        if let Some(dir) = app.cache_dir.as_ref() {
            let p = dir.join("index.bin");
            let _ = search_index::save(&app.doc_index, &p);
        }
        app.index_persisted = true;
    }
}

/// Render + bake overlay + pre-encode PNG for one upcoming uncached
/// page, and proactively transmit the bitmap to the terminal so the
/// next draw is pure placement (no payload IO). Returns `true` if a
/// page was warmed, `false` if the prefetch window held no eligible
/// candidate (depth exhausted, all already cached, or all failed).
///
/// Why pre-transmit during idle (not just pre-encode): the next draw
/// cycle would otherwise pay ~2 ms of pty-write to ship the cached
/// bytes. By writing them now while the user is idle, scroll-cycle
/// cost drops to placement-only (~1 ms total). The runtime safety
/// invariant — that we only write to stdout outside of `term.draw`
/// — holds because `warm_one_idle` is only called from the idle
/// branch of `run_loop`.
fn warm_next_uncached(app: &mut App<'_>) -> Result<bool> {
    let viewport_h = app.viewport_px.1;
    if viewport_h == 0 {
        return Ok(false);
    }
    let visible = app.layout.visible_pages(app.scroll_y_px, viewport_h);
    if visible.is_empty() {
        return Ok(false);
    }
    let dir: i64 = if app.last_scroll_dir >= 0 { 1 } else { -1 };
    let start: i64 = if dir > 0 {
        visible.end as i64
    } else {
        visible.start as i64 - 1
    };
    // Look up to 8 pages ahead in the scroll direction for the first
    // un-cached one. With `MAX_WARMS_PER_IDLE` calls per idle window
    // the absolute reachable distance is depth(=8) but typically the
    // first 1-2 pages encountered will be the ones to warm.
    const PREFETCH_DEPTH: i64 = 8;
    let fit_width_px = app.layout.fit_width_px;
    for offset in 0..PREFETCH_DEPTH {
        let cand = start + offset * dir;
        if cand < 0 || (cand as usize) >= app.page_count {
            break;
        }
        let pi = cand as usize;
        if app.page_cache.contains_key(&pi) || app.failed_pages.contains(&pi) {
            continue;
        }
        ui::ensure_page_rendered(app, pi, fit_width_px, /*allow_failure=*/ true)?;
        let layout_key = crate::app::LayoutKey {
            fit_width_px,
            dark: app.dark,
        };
        ui::ensure_overlay(app, pi, layout_key);
        let revision = ui::compute_page_revision(app, pi);
        // Build the transmit string from the cached payload (the
        // build_transmit method primes its own cache) and ship it
        // directly to stdout. mark_transmitted afterwards so the next
        // draw's is_fresh check returns true and skips the transmit.
        //
        // Source the transmit bitmap from the highlights-baked tier
        // when present, else fall back to page_cache.as_rgba8() (a
        // borrowed view of the pdfium-returned bitmap). Selection is
        // intentionally NOT baked into either source — in kitty mode
        // it ships as a separate layered overlay, so the page bitmap
        // is selection-stable across selection moves.
        let bm: Option<&image::RgbaImage> = app.highlights_baked_cache.get(&pi)
            .map(|(bm, _)| bm)
            .or_else(|| app.page_cache.get(&pi).and_then(|d| d.as_rgba8()));
        let pixel_dims = bm.map(|bm| (bm.width(), bm.height()));
        let kp = app.kitty_pages.as_mut();
        if let (Some(kp), Some(bm), Some((w, h))) = (kp, bm, pixel_dims) {
            let transmit = kp.build_transmit(bm, pi, layout_key, revision);
            // Write directly to stdout. We're outside of term.draw so
            // no interleaving with ratatui's own writes — io::Stdout
            // is line-/byte-flushable and acquires its own lock per
            // call; we flush explicitly so the terminal sees the
            // bytes before the next draw.
            use std::io::Write;
            let mut stdout = io::stdout().lock();
            if stdout.write_all(transmit.as_bytes()).is_ok() && stdout.flush().is_ok() {
                kp.mark_transmitted(pi, layout_key, revision, w, h);
            }
        }
        return Ok(true);
    }
    Ok(false)
}

/// Same as `dispatch_event` but with explicit per-event coalescing
/// for two high-rate event kinds:
///
/// 1. **Mouse drags** — fire 30-100 events/sec. Each Drag mutates
///    `text_selection.head` and triggers a recompose; the user only
///    cares about the final position.
///
/// 2. **Mouse wheel scrolls** — modern wheels and trackpads emit 5-10
///    `ScrollUp`/`ScrollDown` events per detent at 60-120 Hz. Without
///    coalescing, each one drives a full recompose + buffer redraw,
///    even though the only meaningful state delta is the cumulative
///    vertical offset. Drain the run, sum net up vs. down, apply one
///    `scroll_by_screens` call.
fn dispatch_event_coalesced(app: &mut App<'_>, ev: Event) -> Result<()> {
    use crossterm::event::{MouseEvent, MouseEventKind};
    if let Event::Mouse(MouseEvent { kind: MouseEventKind::Drag(btn), .. }) = ev {
        // Drain consecutive Drag(btn) events; keep only the last.
        let mut latest = ev;
        while event::poll(Duration::ZERO)? {
            // We can't peek without consuming; read and check.
            let next = event::read()?;
            match &next {
                Event::Mouse(MouseEvent { kind: MouseEventKind::Drag(b2), .. }) if *b2 == btn => {
                    latest = next;
                }
                _ => {
                    // Non-drag event broke the run — dispatch the latest drag,
                    // then this event normally.
                    dispatch_event(app, latest)?;
                    return dispatch_event(app, next);
                }
            }
        }
        return dispatch_event(app, latest);
    }
    if let Event::Mouse(me0) = ev {
        if matches!(me0.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) {
            // Sum net vertical scroll across this run. Shifted scroll
            // is horizontal — keep separate so we don't merge h/v.
            let shifted = me0.modifiers.contains(crossterm::event::KeyModifiers::SHIFT);
            let mut net = match me0.kind {
                MouseEventKind::ScrollDown => 1i32,
                MouseEventKind::ScrollUp => -1i32,
                _ => 0,
            };
            while event::poll(Duration::ZERO)? {
                let next = event::read()?;
                if let Event::Mouse(me) = next {
                    let m_shift = me.modifiers.contains(crossterm::event::KeyModifiers::SHIFT);
                    if m_shift == shifted {
                        match me.kind {
                            MouseEventKind::ScrollDown => { net += 1; continue; }
                            MouseEventKind::ScrollUp => { net -= 1; continue; }
                            _ => {}
                        }
                    }
                    // Different modifier or non-scroll mouse event: flush
                    // the coalesced scroll first, then dispatch normally.
                    apply_coalesced_scroll(app, net, shifted);
                    return dispatch_event(app, Event::Mouse(me));
                }
                // Non-mouse event (key / resize): flush + dispatch.
                apply_coalesced_scroll(app, net, shifted);
                return dispatch_event(app, next);
            }
            apply_coalesced_scroll(app, net, shifted);
            return Ok(());
        }
    }
    dispatch_event(app, ev)
}

fn apply_coalesced_scroll(app: &mut App<'_>, net: i32, shifted: bool) {
    if net == 0 {
        return;
    }
    let amount = net as f32 * keys::SCROLL_LINE;
    if shifted {
        app.scroll_x_by(amount);
    } else {
        app.scroll_by_screens(amount);
    }
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

#[cfg(test)]
mod idle_tier_tests {
    use super::*;

    #[test]
    fn no_input_yet_skips_all_idle_work() {
        // The pre-fix behaviour was `unwrap_or(true)`, which kicked
        // off prefetch + text indexing on a freshly-opened PDF before
        // the user had touched the keyboard. That produced ~40% CPU
        // and a tmux passthrough flood that crashed Ghostty on big
        // books. Skip is what protects us now.
        assert_eq!(idle_action(None), IdleAction::Skip);
    }

    #[test]
    fn below_bitmap_threshold_skips() {
        assert_eq!(
            idle_action(Some(Duration::from_millis(IDLE_BITMAP_WARM_MS - 1))),
            IdleAction::Skip
        );
    }

    #[test]
    fn at_bitmap_threshold_warms_bitmap_only() {
        assert_eq!(
            idle_action(Some(Duration::from_millis(IDLE_BITMAP_WARM_MS))),
            IdleAction::BitmapWarm
        );
    }

    #[test]
    fn between_thresholds_warms_bitmap_only() {
        // Active reading cadence (key every few hundred ms) must NOT
        // trigger the heavier text-index work — that's what blew the
        // CPU budget on long sessions.
        assert_eq!(
            idle_action(Some(Duration::from_millis(IDLE_TEXT_INDEX_MS - 1))),
            IdleAction::BitmapWarm
        );
    }

    #[test]
    fn at_text_index_threshold_runs_both_tiers() {
        assert_eq!(
            idle_action(Some(Duration::from_millis(IDLE_TEXT_INDEX_MS))),
            IdleAction::BitmapWarmAndTextIndex
        );
    }

    #[test]
    fn long_idle_runs_both_tiers() {
        assert_eq!(
            idle_action(Some(Duration::from_secs(30))),
            IdleAction::BitmapWarmAndTextIndex
        );
    }

    // Compile-time consistency: bitmap tier must trigger before (or
    // at the same time as) the text tier — otherwise text-indexing
    // without prior bitmap-warming would happen, contradicting the
    // policy promise that text-index is strictly heavier work.
    const _THRESHOLD_ORDER_GUARD: () = {
        assert!(IDLE_BITMAP_WARM_MS <= IDLE_TEXT_INDEX_MS);
    };

    // Compile-time guard on the warm budget: caps the per-tick burst
    // that previously hit 4 PNG transmits through tmux passthrough
    // back-to-back. If a future commit bumps this above 3, the build
    // fails so the tmux/Ghostty risk gets re-evaluated explicitly.
    const _WARM_BUDGET_GUARD: () = {
        assert!(MAX_WARMS_PER_IDLE >= 1);
        assert!(MAX_WARMS_PER_IDLE <= 3);
    };
}
