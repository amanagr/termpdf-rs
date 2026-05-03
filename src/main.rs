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
    let res = run_loop(&mut app);
    teardown_terminal()?;
    profile::report();

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
        out.display(),
        page + 1,
        dark
    );
    Ok(())
}

fn run_loop(app: &mut App<'_>) -> Result<()> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut term = Terminal::new(backend)?;
    // Watchdog: minimum interval between actual paints under sustained
    // input. 16 ms ≈ 60 Hz. Lets a held-`j` collapse multiple steps
    // into one paint while keeping single keys feeling responsive.
    const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(16);
    /// Must match the `RAPID_SCROLL_THRESHOLD_MS` in
    /// `App::is_rapid_scrolling`. Quick enough that letting up a held
    /// key feels instant; long enough that brief pauses during fast
    /// keymashing don't trigger an expensive catch-up draw mid-burst.
    const SETTLE_MS: u128 = 120;
    let mut last_draw = std::time::Instant::now() - MIN_FRAME_INTERVAL;

    // After a rapid-input burst settles, we want to schedule one
    // catch-up draw that does the deferred cold-page transmits.
    // Tracked here in the loop rather than per-event so we don't
    // forget if multiple events arrive within the threshold.
    let mut needs_settle_redraw = false;

    while !app.should_quit {
        // Pre-draw peek: if more input is already queued, don't paint
        // the intermediate state — drain first, then paint the result.
        // The watchdog guarantees we still paint at ~60 Hz so very
        // long input bursts (mouse drags, autorepeat) don't lock the
        // screen.
        let mut should_draw = true;
        let now = std::time::Instant::now();
        if now.duration_since(last_draw) < MIN_FRAME_INTERVAL && event::poll(Duration::ZERO)? {
            should_draw = false;
        }
        if should_draw {
            let _draw = profile::span(profile::Phase::Draw);
            term.draw(|f| ui::draw(f, app))?;
            drop(_draw);
            last_draw = std::time::Instant::now();
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
        if event::poll(poll_dur)? {
            dispatch_event_coalesced(app, event::read()?)?;
            app.note_input();
            needs_settle_redraw = true;
            while !app.should_quit && event::poll(Duration::ZERO)? {
                dispatch_event_coalesced(app, event::read()?)?;
                app.note_input();
            }
        } else if needs_settle_redraw {
            // Poll timed out → input is idle. The next iteration's
            // draw will see is_rapid_scrolling() == false and do the
            // deferred transmits. Force the watchdog to allow that
            // draw immediately.
            needs_settle_redraw = false;
            last_draw = std::time::Instant::now() - MIN_FRAME_INTERVAL;
        } else {
            // True idle: no input pending and no settle to do. Use
            // the moment to warm one upcoming page so the next j/k
            // press hits the cache instead of a 20 ms pdfium render.
            // Bounded to one page per idle tick so input lag is at
            // most one pdfium call.
            let idle_long_enough = app
                .last_input_at
                .map(|t| t.elapsed() >= Duration::from_millis(200))
                .unwrap_or(true);
            if idle_long_enough {
                let _s = profile::span(profile::Phase::IdleWarm);
                let _ = warm_one_idle(app);
            }
        }
    }
    Ok(())
}

/// Idle-time prefetch. Renders + pre-encodes upcoming pages in the
/// user's scroll direction so the next j/k press lands on a cached
/// page instead of a synchronous ~20 ms pdfium call.
///
/// Loops up to `MAX_WARMS_PER_IDLE` times in a single call, polling
/// for input between iterations so a keystroke mid-prefetch breaks
/// out immediately. Each iteration costs ~25 ms (pdfium + overlay +
/// PNG encode); 4 of them packs a whole prefetch tier into ~100 ms
/// of idle slack.
///
/// Also opportunistically extends the doc-text index (for fast
/// search) one page per call. The text extraction is cheap relative
/// to a full pdfium render, so it's a free side-effect of being on
/// the page-warming critical path.
fn warm_one_idle(app: &mut App<'_>) -> Result<()> {
    const MAX_WARMS_PER_IDLE: u32 = 4;
    for _ in 0..MAX_WARMS_PER_IDLE {
        if !warm_next_uncached(app)? {
            // No bitmap warming work — but maybe text indexing has
            // leftovers (we cap text-only work to one per idle call
            // so it doesn't starve the bitmap warm budget).
            index_one_page_text(app);
            return Ok(());
        }
        if event::poll(Duration::ZERO)? {
            return Ok(());
        }
    }
    // After bitmap warms, pump one text-index entry too if there's
    // still slack.
    index_one_page_text(app);
    Ok(())
}

/// Extract text for the next un-indexed page and add to the search
/// index. ~5 ms per page on a typical doc. Bounded to one call per
/// `warm_one_idle` so a held-key burst that interrupts us costs at
/// most one text extract.
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
    // un-cached one. With MAX_WARMS_PER_IDLE=4 calls per idle window
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
        // ensure_overlay only inserts into overlay_cache when the
        // selection touches the page — for any other page we'd find
        // an empty overlay_cache slot. Fall back to highlights_baked
        // (which ensure_overlay always populates) so the warm tick
        // still ships a payload to the terminal.
        let bm = app
            .overlay_cache
            .get(&pi)
            .map(|(bm, _)| bm)
            .or_else(|| app.highlights_baked_cache.get(&pi).map(|(bm, _)| bm));
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
/// for mouse drags: when a `Drag` is dispatched, peek for more drags
/// behind it and skip the intermediate ones. Each Drag mutates
/// `text_selection.head` and triggers a recompose; the user only
/// cares about the final position.
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
    dispatch_event(app, ev)
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
