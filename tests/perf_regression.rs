//! End-to-end perf-behavior regressions.
//!
//! Asserts that the perf optimizations stay wired into the binary —
//! catches the class of refactor that drops a `disk_cache::store(...)`
//! call or breaks the `warm_one_idle` plumbing without touching any
//! unit-tested function.
//!
//! What this verifies (vs unit tests for the underlying logic):
//!   1. `ensure_page_rendered` writes PNG files into XDG_CACHE_HOME
//!      after rendering — i.e. the disk-cache `store(...)` call
//!      is still wired into the kitty draw path.
//!   2. Idle warm pre-transmits more kitty `_G` headers than the
//!      number of pages initially visible — proves `warm_one_idle`
//!      runs, hits `build_transmit`, and writes to stdout.
//!
//! Separate test binary (own pdfium binding); reuses the helper
//! pattern from `render_kitty.rs` rather than sharing via a
//! `tests/common/` module to keep the existing files untouched.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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

fn pdfium_or_skip() -> Option<pdfium_render::prelude::Pdfium> {
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
    let lib = candidates
        .into_iter()
        .flatten()
        .find(|p| std::path::Path::new(p).exists());
    let lib = match lib {
        Some(p) => p,
        None => {
            eprintln!("skipping: libpdfium not found");
            return None;
        }
    };
    let bindings = pdfium_render::prelude::Pdfium::bind_to_library(&lib).ok()?;
    Some(pdfium_render::prelude::Pdfium::new(bindings))
}

/// Build a 5-page test PDF so the binary has reachable un-cached
/// pages for the idle warm path to grab.
fn make_test_pdf(pdfium: &pdfium_render::prelude::Pdfium, n_pages: usize) -> PathBuf {
    use pdfium_render::prelude::*;
    let mut path = std::env::temp_dir();
    path.push(format!(
        "termpdf-perf-regression-{}.pdf",
        std::process::id()
    ));
    let mut doc = pdfium.create_new_pdf().expect("create pdf");
    for i in 0..n_pages {
        let mut page = doc
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::a4())
            .expect("create page");
        let font = doc.fonts_mut().helvetica();
        let text = format!("page {}", i + 1);
        let mut object =
            PdfPageTextObject::new(&doc, text, font, PdfPoints::new(14.0)).expect("text");
        object
            .translate(PdfPoints::new(50.0), PdfPoints::new(700.0))
            .expect("translate");
        page.objects_mut().add_text_object(object).expect("add");
    }
    doc.save_to_file(&path).expect("save");
    path
}

fn spawn_in_pty(
    pdf: &Path,
    scratch_cache: &Path,
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
    cmd.arg("kitty");
    cmd.env("TERM", "xterm-kitty");
    cmd.env_remove("TMUX");
    cmd.env(
        "TERMPDF_PDFIUM",
        format!("{}/vendor/lib/libpdfium.so", env!("CARGO_MANIFEST_DIR")),
    );
    cmd.env("TERMPDF_CELL_PX", "8x16");
    // Sandbox the disk cache so this test owns its own XDG_CACHE_HOME
    // and we can directly inspect what the binary wrote.
    cmd.env("XDG_CACHE_HOME", scratch_cache);

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

/// Count occurrences of a first-chunk kitty transmit header. Each
/// FIRST chunk of an image starts with `\x1b_Gq=1,i=…,a=T,…`; later
/// chunks of the same image carry only `\x1b_Gm=…`. We count the
/// `q=1,i=` prefix because it appears once per image and not on the
/// continuation chunks. (The byte signature is what changed when we
/// flipped q=2 → q=1; q=2 was wrong-direction per the kitty spec.)
fn count_kitty_transmit_headers(haystack: &[u8]) -> usize {
    let needle = b"\x1b_Gq=1,i=";
    if needle.len() > haystack.len() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

fn count_png_files_recursive(root: &Path) -> usize {
    let mut n = 0;
    let Ok(read) = std::fs::read_dir(root) else {
        return 0;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            n += count_png_files_recursive(&path);
        } else if path.extension().is_some_and(|e| e == "png") {
            n += 1;
        }
    }
    n
}

/// Combined regression test: opens a 5-page PDF, waits for the idle
/// warm budget to fire a couple of cycles, then quits and inspects
/// the artifacts.
///
/// Why combined: every #[test] in this file would create a fresh
/// pdfium binding via `pdfium_or_skip`, but `bind_to_library` is
/// process-global and parallel tests would contend. Keeping a
/// single test gives us one binding + multiple assertions and
/// matches the pattern in `render_kitty.rs`.
#[test]
fn perf_optimizations_stay_wired_into_binary() {
    let Some(pdfium) = pdfium_or_skip() else {
        return;
    };
    let pdf = make_test_pdf(&pdfium, 5);

    let scratch_cache =
        std::env::temp_dir().join(format!("perf-regression-cache-{}", std::process::id()));
    // Best-effort cleanup of any leftover scratch from a prior crashed
    // run — set_var/remove_var won't fail us either way.
    let _ = std::fs::remove_dir_all(&scratch_cache);
    std::fs::create_dir_all(&scratch_cache).expect("create scratch cache");

    let (mut child, mut writer, rx) = spawn_in_pty(&pdf, &scratch_cache);

    // Wait for first paint so we know the binary started OK.
    let mut buf = Vec::new();
    let painted = drain_until(&rx, &mut buf, Duration::from_secs(8), |b| {
        twoway_contains(b, b"1/5")
    });
    assert!(painted, "first paint (`1/5` status) never arrived");

    // The visible region of a 30-row × 8×16-cell viewport is ~480 px
    // tall; an A4 page rendered at 800 px wide is ~1131 px tall, so
    // exactly ONE page is visible at a time. The initial draw should
    // therefore emit a single transmit for that one page.
    let initial_transmits = count_kitty_transmit_headers(&buf);

    // Idle prefetch only kicks in *after* the user has registered at
    // least one input — otherwise opening a fresh book would burst
    // pdfium renders + tmux passthrough kitty transmits before the
    // user has done anything (which is what crashed Ghostty on big
    // books). Send a benign no-op key so the run-loop's
    // `last_input_at` becomes Some without changing visible state.
    // `g` alone is the start of a `gg` chord — it's recorded as
    // input but doesn't scroll or render anything new.
    writer.write_all(b"g").ok();
    writer.flush().ok();

    // Now sit idle for ~3 s. The first ~200 ms is below the
    // bitmap-warm threshold; from there on, the warm budget of 2
    // pages per 250 ms idle window pre-encodes and pre-transmits
    // the remaining cold pages to stdout.
    drain_until(&rx, &mut buf, Duration::from_secs(3), |_| false);

    let after_idle_transmits = count_kitty_transmit_headers(&buf);

    // === Assertion 3 (added later) baseline marker ===
    //
    // Snapshot the byte count NOW. The warm pipeline (page warmups +
    // Sharp upgrade re-transmits) is fully drained — every cold page
    // in the 5-page test PDF has been warmed and the visible page
    // upgraded to Sharp. Any bytes after this point would mean the
    // idle path is still poking the terminal — which is exactly the
    // class of regression that drove Ghostty CPU to 50-90 % when the
    // user had a PDF open and "nothing" happening.
    let bytes_after_warm_settled = buf.len();

    // Now drain another 3 s of *fully-settled* idle. Anything that
    // arrives in this window is a regression: term.draw firing on a
    // periodic timer, an idle warm path that re-transmits cached
    // pages, etc. Tolerate up to 4 KB to absorb tiny things like a
    // single HUD cell update — but no more.
    drain_until(&rx, &mut buf, Duration::from_secs(3), |_| false);
    let bytes_during_quiet_window = buf.len() - bytes_after_warm_settled;

    // Quit cleanly so we can inspect the disk cache without races
    // against the still-writing process.
    writer.write_all(b":q\r").ok();
    writer.flush().ok();
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
    assert!(clean_exit, "binary did not exit on `:q\\r`");

    // === Assertion 1: idle warm pre-transmits fire ===
    //
    // Initial paint already emitted ≥1 transmit. After 2 s of idle
    // we should have meaningfully more — even allowing for one
    // missed cycle and the prefetch depth not catching every
    // remaining cold page, idle warm should add at least 2 more.
    assert!(
        after_idle_transmits >= initial_transmits + 2,
        "idle warm pre-transmit appears unwired: \
         {initial_transmits} transmits at first paint, \
         {after_idle_transmits} after 2s idle (expected ≥+2). \
         If `warm_one_idle` no longer calls `build_transmit` and \
         writes to stdout, this assertion fails."
    );

    // === Assertion 3: idle is silent ===
    //
    // After the warm pipeline drains, the binary should write
    // essentially nothing to the pty until the user does something.
    // The dirty-flag in run_loop is what guarantees this — without
    // it, term.draw fires on every 250 ms idle poll and ratatui's
    // backend flushes whatever it has, which in practice means
    // Ghostty's CPU climbs to 50-90 % even when the user is doing
    // nothing. The 4 KB ceiling absorbs incidental writes (e.g. one
    // HUD cell update) without being so loose that a real regression
    // (per-frame placement re-emit) slips through.
    const IDLE_BYTES_CEIL: usize = 4096;
    assert!(
        bytes_during_quiet_window <= IDLE_BYTES_CEIL,
        "fully-settled idle is no longer silent: {bytes_during_quiet_window} bytes \
         emitted in 3 s after the warm pipeline drained (ceiling = \
         {IDLE_BYTES_CEIL} B). If you see this, something in the run-loop is \
         calling term.draw on a periodic timer, or warm_one_idle is re-transmitting \
         pages forever. Look at the `dirty` gating in src/main.rs::run_loop."
    );

    // === Assertion 2: disk cache writes happen ===
    //
    // With XDG_CACHE_HOME pointed at our scratch dir, every
    // ensure_page_rendered call should `disk_cache::store(...)` a
    // PNG. After running for several seconds across 5 pages, expect
    // at least one PNG to land. (Exact count varies with the warm
    // budget across the test's run window, so we just assert ≥1.)
    let cache_root = scratch_cache.join("termpdf-rs");
    assert!(
        cache_root.exists(),
        "$XDG_CACHE_HOME/termpdf-rs/ never created at {}; \
         disk_cache::store(...) appears unwired",
        cache_root.display()
    );
    let png_count = count_png_files_recursive(&cache_root);
    assert!(
        png_count >= 1,
        "expected ≥1 PNG written under {}, found {png_count}. \
         If `ensure_page_rendered` no longer calls disk_cache::store, \
         this assertion fails.",
        cache_root.display()
    );

    // Best-effort cleanup; failures here don't matter for the test.
    let _ = std::fs::remove_dir_all(&scratch_cache);
    let _ = std::fs::remove_file(&pdf);
}
