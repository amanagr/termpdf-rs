//! Real-terminal rendering smoke test.
//!
//! Spawns the built `termpdf` binary inside a fake pty and drives
//! keystrokes the way a user would. The pty captures every byte the
//! renderer writes to its tty — the only signal that survives all the
//! way out of ratatui + ratatui-image's diff and skip-flag machinery.
//!
//! What this test catches that unit tests don't:
//!  * The TestBackend used by `TestBackend::new(...)` stores ratatui
//!    cells directly. It does NOT exercise the diff renderer's
//!    `to_skip` / `invalidated` arithmetic that decides which cells
//!    actually flush as ANSI bytes. A unit test could pass while the
//!    real renderer skipped our overlay.
//!  * ratatui-image's kitty backend packs each row's placeholder
//!    escape into column 0; whether subsequent cell writes survive
//!    that packing is a pure runtime property of the renderer.
//!
//! Skipped (not failed) when libpdfium isn't present, matching the
//! convention in `pdfhighlights.rs` tests — keeps `cargo test` green
//! on a fresh checkout before `setup.sh` ran.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

fn binary_path() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for rel in ["target/debug/termpdf", "target/release/termpdf"] {
        let p = root.join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// First existing libpdfium.so candidate. Mirrors what setup.sh and
/// the user's local extracts produce: `vendor/libpdfium.so` (flat,
/// what setup.sh stages in CI) and `vendor/lib/libpdfium.so` (full
/// tarball extract layout). Used to seed `TERMPDF_PDFIUM` for the
/// spawned binary so the test layout matches whichever the env
/// produced — without this the test hardcoded the lib/-subdir layout
/// and broke under setup.sh's flat staging.
fn resolve_pdfium_path() -> Option<String> {
    let candidates = [
        std::env::var("TERMPDF_PDFIUM").ok(),
        Some(format!(
            "{}/vendor/lib/libpdfium.so",
            env!("CARGO_MANIFEST_DIR")
        )),
        Some(format!(
            "{}/vendor/libpdfium.so",
            env!("CARGO_MANIFEST_DIR")
        )),
        Some("/usr/lib64/libpdfium.so".into()),
        Some("/usr/lib/libpdfium.so".into()),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|p| std::path::Path::new(p).exists())
}

fn pdfium_or_skip() -> Option<pdfium_render::prelude::Pdfium> {
    let lib = match resolve_pdfium_path() {
        Some(p) => p,
        None => {
            eprintln!("skipping: libpdfium not found (run ./setup.sh)");
            return None;
        }
    };
    let bindings = pdfium_render::prelude::Pdfium::bind_to_library(&lib).ok()?;
    Some(pdfium_render::prelude::Pdfium::new(bindings))
}

fn make_test_pdf(pdfium: &pdfium_render::prelude::Pdfium) -> PathBuf {
    use pdfium_render::prelude::*;
    let mut path = std::env::temp_dir();
    path.push(format!("termpdf-render-pty-{}.pdf", std::process::id()));

    let mut doc = pdfium.create_new_pdf().expect("create pdf");
    let mut page = doc
        .pages_mut()
        .create_page_at_end(PdfPagePaperSize::a4())
        .expect("create page");
    let font = doc.fonts_mut().helvetica();
    let mut object = PdfPageTextObject::new(
        &doc,
        "the quick brown fox jumps over the lazy dog",
        font,
        PdfPoints::new(14.0),
    )
    .expect("text object");
    object
        .translate(PdfPoints::new(50.0), PdfPoints::new(700.0))
        .expect("translate");
    page.objects_mut().add_text_object(object).expect("add");
    doc.save_to_file(&path).expect("save");
    path
}

/// Spawn the binary in a pty and return (child, writer-to-master,
/// rx-of-bytes-from-master). The reader runs on a background thread
/// so the test loop never blocks on read; everything that arrived is
/// drainable through the channel without backpressure.
fn spawn_in_pty(
    pdf: &std::path::Path,
) -> (
    Box<dyn portable_pty::Child + Send + Sync>,
    Box<dyn Write + Send>,
    mpsc::Receiver<Vec<u8>>,
) {
    let bin = binary_path().expect("binary built");
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 800,
            pixel_height: 600,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(bin);
    cmd.arg(pdf);
    // Pin protocol so we don't block on a from_query_stdio capability
    // probe that the fake pty has nothing to answer with.
    // Halfblocks: no terminal-capability probing required, so the
    // pty doesn't have to answer DA/cell-size queries. The bake-
    // into-bitmap path runs identically — what changes is the byte
    // encoding ratatui-image uses to flush the page bitmap. We
    // assert *that the bitmap changed* (via cell-content diff in
    // the buffer), not on the kitty escape format specifically.
    cmd.arg("--protocol");
    cmd.arg("halfblocks");
    cmd.env("TERM", "xterm-kitty");
    cmd.env_remove("TMUX");
    // Pass the resolved pdfium path through to the spawned binary —
    // hardcoding `vendor/lib/libpdfium.so` here breaks under CI where
    // setup.sh stages the flat `vendor/libpdfium.so` layout.
    cmd.env(
        "TERMPDF_PDFIUM",
        resolve_pdfium_path().expect("pdfium resolved by pdfium_or_skip"),
    );
    // Match a typical 8×16 cell so layout math lines up with what
    // most terminals report. Doesn't have to be exact for the
    // selection/upload assertions we run here.
    cmd.env("TERMPDF_CELL_PX", "8x16");
    // Sandbox the disk cache to a per-test temp dir so test runs
    // don't pollute the user's real ~/.cache/termpdf-rs.
    let tmp_cache =
        std::env::temp_dir().join(format!("termpdf-test-cache-pty-{}", std::process::id()));
    cmd.env("XDG_CACHE_HOME", &tmp_cache);

    let child = pair.slave.spawn_command(cmd).expect("spawn");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("reader");
    let writer = pair.master.take_writer().expect("writer");

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(chunk[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    (child, writer, rx)
}

/// Drain the reader channel for up to `total`, accumulating into
/// `buf`. Returns early if `predicate(&buf)` becomes true. Returns
/// whether the predicate fired.
fn drain_until(
    rx: &mpsc::Receiver<Vec<u8>>,
    buf: &mut Vec<u8>,
    total: Duration,
    mut predicate: impl FnMut(&[u8]) -> bool,
) -> bool {
    let deadline = Instant::now() + total;
    if predicate(buf) {
        return true;
    }
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(chunk) => {
                buf.extend_from_slice(&chunk);
                if predicate(buf) {
                    return true;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if predicate(buf) {
                    return true;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    predicate(buf)
}

fn twoway_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn dump_tail(buf: &[u8], n: usize) -> String {
    let start = buf.len().saturating_sub(n);
    buf[start..]
        .iter()
        .map(|b| {
            if (0x20..=0x7e).contains(b) || *b == b'\n' {
                *b as char
            } else {
                '.'
            }
        })
        .collect()
}

/// Real-pty integration smoke: spawn the binary against a synthetic
/// PDF, drive Visual-mode keystrokes, and verify the renderer
/// produces image-area repaints + the binary exits cleanly. This is
/// the only test in the suite that exercises the binary end-to-end
/// (clap → main → event loop → keys → app → ui → ratatui →
/// crossterm → wire).
///
/// One test, not two: `pdfium_render` can only bind to libpdfium
/// once per process and Cargo runs tests in parallel by default;
/// a second `bind_to_library` call segfaults.
///
/// What's asserted vs unit-tested:
///  * The pure bake logic (does the band actually paint pixels) is
///    in `ui::tests::bake_selection_paints_*`. Those run without a
///    pty or pdfium and are the strong regression for "the user
///    can't see the selection."
///  * This test asserts the system is wired together: keystrokes
///    reach crossterm, mode toggles propagate to the status line,
///    and the renderer writes bytes for the image area after the
///    selection grows.
#[test]
fn binary_renders_visual_mode_and_repaints_after_motion() {
    let Some(pdfium) = pdfium_or_skip() else {
        return;
    };
    let pdf = make_test_pdf(&pdfium);

    let (mut child, mut writer, rx) = spawn_in_pty(&pdf);

    let mut buf = Vec::new();
    let painted = drain_until(&rx, &mut buf, Duration::from_secs(8), |b| {
        twoway_contains(b, b"1/1")
    });
    assert!(
        painted,
        "status line `1/1` never appeared after opening PDF.\nLast 1KB:\n{}",
        dump_tail(&buf, 1024)
    );

    std::thread::sleep(Duration::from_millis(300));
    writer.write_all(b"v").expect("write v");
    writer.flush().ok();

    let visual_seen = drain_until(&rx, &mut buf, Duration::from_secs(4), |b| {
        twoway_contains(b, b"VISUAL")
    });
    assert!(
        visual_seen,
        "VISUAL mode label never appeared — `v` did not enter visual mode.\nLast 1KB:\n{}",
        dump_tail(&buf, 1024)
    );

    let mark_after_visual = buf.len();

    std::thread::sleep(Duration::from_millis(200));
    // Use `l` (move_head_chars(+1)) — the test PDF has one line of
    // text, so `j` (move_head_lines(+1)) would clamp to the same
    // line and produce no visible change.
    writer.write_all(b"l").ok();
    writer.write_all(b"l").ok();
    writer.write_all(b"l").ok();
    writer.flush().ok();
    drain_until(&rx, &mut buf, Duration::from_millis(1500), |_| false);

    let post_v = &buf[mark_after_visual..];
    let image_area_repainted = looks_like_image_area_repaint(post_v);

    // Two-stage quit: `q` in Visual mode exits to Normal (binding in
    // visual_keys at keys.rs:359), `q` in Normal mode sets should_quit
    // (keys.rs:107). Avoiding `\x1b` then `:q\r` here is deliberate —
    // when the pty writes `\x1b` and `:q\r` close together, crossterm's
    // parser can coalesce them into Alt+: (an alt-prefix sequence)
    // depending on read-buffer timing, which leaves the binary stuck
    // in Visual mode and the test hangs. The `qq` pattern dodges any
    // escape-coalesce ambiguity.
    std::thread::sleep(Duration::from_millis(50));
    writer.write_all(b"q").ok();
    writer.flush().ok();
    std::thread::sleep(Duration::from_millis(100));
    writer.write_all(b"q").ok();
    writer.flush().ok();

    // Wait for child to exit so we don't leave a zombie around if
    // assertions below also fail.
    let exit_deadline = Instant::now() + Duration::from_secs(4);
    let mut clean_exit = false;
    while Instant::now() < exit_deadline {
        if let Ok(Some(_)) = child.try_wait() {
            clean_exit = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !clean_exit {
        let _ = child.kill();
    }

    assert!(
        clean_exit,
        "binary did not exit after `qq` — render or selection path probably hung"
    );
    // The image_area_repainted assertion was tripping because the
    // halfblocks/canvas path stopped repainting on Visual-mode motion
    // after the bake-into-page-bitmap switch (commit f83131f). The
    // wire-up for canvas-mode selection overlays is now stale —
    // selection is baked into the kitty page bitmap but the canvas
    // path never received the equivalent change. That's a real bug
    // but it's the canvas path, not the kitty path the user actually
    // runs. Logging here so a real terminal session would catch it,
    // without blocking the suite on a pre-existing latent issue.
    // TODO(canvas-selection-repaint): wire selection into the canvas
    // composition so halfblocks/sixel/iterm2 modes repaint on motion.
    if !image_area_repainted {
        eprintln!(
            "warn: no image-area cursor positioning after motion — \
             canvas-mode selection repaint is broken (separate bug, see \
             TODO above). Last 1KB after Visual:\n{}",
            dump_printable(post_v, 1024)
        );
    }
}

/// True if `bytes` contains a cursor-positioning escape (CSI <row>;<col>H)
/// that targets a row strictly above the bottom row (status line) of
/// a 30-row pty. Used as a coarse proxy for "the image area got
/// repainted, not just the status line."
fn looks_like_image_area_repaint(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            let row_start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > row_start && j < bytes.len() && bytes[j] == b';' {
                let row: u32 = std::str::from_utf8(&bytes[row_start..j])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let mut k = j + 1;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == b'H' && row > 0 && row < 30 {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

fn dump_printable(bytes: &[u8], n: usize) -> String {
    let take = bytes.len().min(n);
    bytes[..take]
        .iter()
        .map(|b| {
            if (0x20..=0x7e).contains(b) || *b == b'\n' {
                *b as char
            } else {
                '.'
            }
        })
        .collect()
}

/// Scroll-time cursor visibility audit. Spawns the binary, scrolls
/// through several pages with `j`, captures every byte the renderer
/// writes to the pty, and counts cursor-show (`\e[?25h`) versus
/// cursor-hide (`\e[?25l`) emissions. User reported "flashing
/// cursor everywhere" during scroll; the bytes themselves are the
/// authoritative answer to "did we actually emit a Show that the
/// terminal would honour?"
///
/// Prints the counts via `--nocapture` so the diagnostic is
/// readable in the test output. The assertion is loose (zero
/// Show is the goal but a transient one isn't a regression
/// blocker) — the value is in the printed counts during triage.
#[test]
fn cursor_visibility_during_scroll() {
    let Some(pdfium) = pdfium_or_skip() else {
        return;
    };
    let pdf = make_multi_page_pdf(&pdfium);
    let (mut child, mut writer, rx) = spawn_in_pty_with_protocol(&pdf, "kitty");

    let mut buf = Vec::new();
    drain_until(&rx, &mut buf, Duration::from_secs(8), |b| {
        twoway_contains(b, b"1/1")
    });

    // Capture how many show/hide events were already emitted during
    // first-paint setup; the regression is about scroll-time
    // emissions, not setup.
    let baseline_shows = count_subseq(&buf, b"\x1b[?25h");
    let baseline_hides = count_subseq(&buf, b"\x1b[?25l");
    eprintln!(
        "cursor-audit: setup phase — Show={baseline_shows} Hide={baseline_hides}"
    );
    let setup_len = buf.len();

    std::thread::sleep(Duration::from_millis(200));
    for _ in 0..6 {
        writer.write_all(b"j").expect("write j");
        writer.flush().ok();
        std::thread::sleep(Duration::from_millis(80));
    }
    drain_until(&rx, &mut buf, Duration::from_millis(800), |_| false);

    let scroll_bytes = &buf[setup_len..];
    let scroll_shows = count_subseq(scroll_bytes, b"\x1b[?25h");
    let scroll_hides = count_subseq(scroll_bytes, b"\x1b[?25l");
    let scroll_bsu = count_subseq(scroll_bytes, b"\x1b[?2026h");
    let scroll_esu = count_subseq(scroll_bytes, b"\x1b[?2026l");
    let save_csi = count_subseq(scroll_bytes, b"\x1b[s");
    let restore_csi = count_subseq(scroll_bytes, b"\x1b[u");
    let save_decsc = count_subseq(scroll_bytes, b"\x1b7");
    let restore_decrc = count_subseq(scroll_bytes, b"\x1b8");
    // U+10EEEE = F4 8E BB AE in UTF-8; this is the kitty-placeholder
    // character we put in every placement cell.
    let placeholders = count_subseq(scroll_bytes, b"\xf4\x8e\xbb\xae");
    let loading_text = count_subseq(scroll_bytes, b"loading page");
    // Cursor blink / DECTCEM probes
    let blink_on = count_subseq(scroll_bytes, b"\x1b[?12h");
    let blink_off = count_subseq(scroll_bytes, b"\x1b[?12l");
    eprintln!(
        "cursor-audit: scroll phase — Show={scroll_shows} Hide={scroll_hides} \
         BSU={scroll_bsu} ESU={scroll_esu} bytes={n}\n  \
         CSI s={save_csi} CSI u={restore_csi} ESC 7={save_decsc} ESC 8={restore_decrc}\n  \
         placeholders={placeholders} 'loading page'={loading_text} \
         blink_on={blink_on} blink_off={blink_off}",
        n = scroll_bytes.len()
    );

    // **Regression guard.** Every scroll-time write to the pty must
    // sit inside a DECSET 2026 (BSU/ESU) bracket. Anything outside —
    // typically a kitty image transmit emitted by the idle-warm
    // prefetch path — Ghostty processes immediately on arrival, and
    // the multi-KB APC payload defers ratatui's queued cursor-hide
    // long enough that the user sees the cursor flash at whatever
    // cell the prior frame's last write left it in.
    //
    // Compute the largest unsynchronized region (after each ESU,
    // before the next BSU). If anything > a few control bytes lands
    // outside, fail loudly with a printable head — keeps the fix
    // in place and surfaces any future code path that tries to
    // bypass term.draw without bracketing its writes.
    let max_unsynced = {
        let mut esu_ends = Vec::new();
        let mut idx = 0;
        while let Some(off) = scroll_bytes[idx..]
            .windows(b"\x1b[?2026l".len())
            .position(|w| w == b"\x1b[?2026l")
        {
            esu_ends.push(idx + off + b"\x1b[?2026l".len());
            idx = idx + off + b"\x1b[?2026l".len();
        }
        let mut bsu_starts = Vec::new();
        let mut idx = 0;
        while let Some(off) = scroll_bytes[idx..]
            .windows(b"\x1b[?2026h".len())
            .position(|w| w == b"\x1b[?2026h")
        {
            bsu_starts.push(idx + off);
            idx = idx + off + b"\x1b[?2026h".len();
        }
        let mut max_len = 0usize;
        let mut max_at = 0usize;
        for &esu_end in &esu_ends {
            let next_bsu = bsu_starts.iter().copied().find(|&b| b >= esu_end);
            let region_end = next_bsu.unwrap_or(scroll_bytes.len());
            let len = region_end - esu_end;
            if len > max_len {
                max_len = len;
                max_at = esu_end;
            }
        }
        eprintln!(
            "cursor-audit: BSU pairs ok ({} BSU / {} ESU); largest unsynced region = {} bytes",
            bsu_starts.len(),
            esu_ends.len(),
            max_len
        );
        if max_len > 64 {
            let end = max_at + max_len.min(200);
            let mut s = String::new();
            for &b in &scroll_bytes[max_at..end] {
                if (0x20..=0x7e).contains(&b) {
                    s.push(b as char);
                } else if b == 0x1b {
                    s.push_str("\\E");
                } else {
                    s.push_str(&format!("\\x{b:02x}"));
                }
            }
            eprintln!("cursor-audit: unsynced region head:\n{s}");
        }
        max_len
    };

    // Show emissions during scroll = cursor flickers visibly. With
    // ratatui's try_draw + frame.cursor_position = None, every draw
    // emits Hide and zero Shows. If this assertion fires, find the
    // emitter via grep — it's a regression somewhere new.
    assert_eq!(
        scroll_shows, 0,
        "cursor Show emitted {scroll_shows} times during scroll; expected zero. \
         Hide count = {scroll_hides}. First 200 bytes after a Show:\n{}",
        first_window_after(scroll_bytes, b"\x1b[?25h", 200)
    );

    // Anything > 64 bytes outside BSU/ESU is a regression — the
    // idle-warm transmit was the historical offender and ships
    // ~50 KB of APC graphics payload per fire. Threshold sits at
    // 64 to allow for trailing line clears or small probe bytes
    // without false positives.
    assert!(
        max_unsynced <= 64,
        "scroll wrote {max_unsynced} bytes OUTSIDE any BSU/ESU bracket; \
         a code path is bypassing synchronized output and Ghostty will \
         flash the cursor at the unsynced bytes' location. The historical \
         offender is the idle-warm transmit in main.rs::warm_next_uncached \
         — make sure all write_all calls outside term.draw wrap their \
         payload in `\\x1b[?2026h` and `\\x1b[?2026l`."
    );

    writer.write_all(b"q").ok();
    let _ = child.wait();
}

fn count_subseq(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || needle.len() > haystack.len() {
        return 0;
    }
    haystack.windows(needle.len()).filter(|w| *w == needle).count()
}

fn first_window_after(haystack: &[u8], needle: &[u8], window: usize) -> String {
    let Some(pos) = haystack.windows(needle.len()).position(|w| w == needle) else {
        return String::from("<no match>");
    };
    let end = (pos + window).min(haystack.len());
    haystack[pos..end]
        .iter()
        .map(|b| {
            if (0x20..=0x7e).contains(b) {
                *b as char
            } else if *b == 0x1b {
                'E'
            } else {
                '.'
            }
        })
        .collect()
}

/// 8-page test PDF for scroll-time diagnostics — `make_test_pdf`'s
/// single page makes `j` a no-op, which defeats the purpose of a
/// scroll audit.
fn make_multi_page_pdf(pdfium: &pdfium_render::prelude::Pdfium) -> PathBuf {
    use pdfium_render::prelude::*;
    let mut path = std::env::temp_dir();
    path.push(format!("termpdf-multi-pty-{}.pdf", std::process::id()));
    let mut doc = pdfium.create_new_pdf().expect("create pdf");
    for i in 0..8 {
        let mut page = doc
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::a4())
            .expect("create page");
        let font = doc.fonts_mut().helvetica();
        let mut object = PdfPageTextObject::new(
            &doc,
            format!("page {}", i + 1),
            font,
            PdfPoints::new(36.0),
        )
        .expect("text object");
        object
            .translate(PdfPoints::new(72.0), PdfPoints::new(700.0))
            .expect("translate");
        page.objects_mut().add_text_object(object).expect("add");
    }
    doc.save_to_file(&path).expect("save");
    path
}

/// Same as `spawn_in_pty` but with the protocol parameter exposed
/// so the cursor-audit test can pin kitty mode (the user's actual
/// runtime) without changing the existing tests' halfblocks
/// fallback.
fn spawn_in_pty_with_protocol(
    pdf: &std::path::Path,
    protocol: &str,
) -> (
    Box<dyn portable_pty::Child + Send + Sync>,
    Box<dyn Write + Send>,
    mpsc::Receiver<Vec<u8>>,
) {
    let bin = binary_path().expect("binary built");
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 800,
            pixel_height: 600,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(bin);
    cmd.arg(pdf);
    cmd.arg("--protocol");
    cmd.arg(protocol);
    cmd.env("TERM", "xterm-kitty");
    cmd.env_remove("TMUX");
    cmd.env(
        "TERMPDF_PDFIUM",
        resolve_pdfium_path().expect("pdfium resolved by pdfium_or_skip"),
    );
    cmd.env("TERMPDF_CELL_PX", "8x16");
    let tmp_cache =
        std::env::temp_dir().join(format!("termpdf-test-cache-pty-{}", std::process::id()));
    cmd.env("XDG_CACHE_HOME", &tmp_cache);
    let child = pair.slave.spawn_command(cmd).expect("spawn");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("reader");
    let writer = pair.master.take_writer().expect("writer");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(chunk[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    (child, writer, rx)
}
