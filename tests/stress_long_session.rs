//! Long-session stress test.
//!
//! Spawns the built `termpdf` binary on a synthetic 200-page doc and
//! drives sustained scrolling for `TERMPDF_STRESS_DURATION_SEC`
//! (default 30 s; CI nightly bumps to 1800 s). Samples RSS at 1 Hz
//! during the run, captures total bytes-on-wire and transmit count,
//! then asserts:
//!
//!   * **Bounded RSS growth**: peak / first-sample < 2.0. Catches the
//!     "we forgot to drop X" memory leaks that only manifest after
//!     thousands of frames.
//!   * **Bounded transmit rate**: total transmits / total seconds <
//!     50 — well below the worst-case sustained scroll cadence
//!     (one cold transmit per j-press, throttled by
//!     SCROLL_THROTTLE_MS), so a regression that retransmits every
//!     frame on idle (the "60 Hz pending_cold_redraw loop"
//!     incident) breaks this immediately.
//!   * **Clean exit**: child responds to `qq` and exits within 5 s
//!     after the scroll loop ends.
//!
//! Skipped (not failed) when:
//!   * `TERMPDF_STRESS_RUN` is unset or empty — keeps `cargo test`
//!     under a minute on developer machines and PR runners.
//!   * libpdfium isn't present (matches the convention in the rest
//!     of the integration suite).
//!
//! Why a separate file from `perf_harness.rs`: perf_harness is short
//! scenarios with hard-baselined metrics. This is one long scenario
//! with soft-thresholded metrics, run intermittently. Different cost
//! profile, different cadence, different consumer.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const DEFAULT_DURATION_SEC: u64 = 30;
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// At sustained-scroll cadence the j-throttle is ~150 ms (per
/// keys.rs SCROLL_THROTTLE_MS) so steady-state max is ~6.7
/// transmits/sec including overlay re-bakes. 50/s is generous
/// headroom — anything above that points at a runaway redraw loop.
const MAX_TRANSMITS_PER_SEC: f64 = 50.0;
/// Allow modest growth — disk_cache LRU + text-index do hold extra
/// state proportional to pages visited. 2.0× initial RSS over 30 s
/// of scrolling 200 pages is a clear leak signal.
const MAX_RSS_GROWTH_RATIO: f64 = 2.0;

fn run_enabled() -> bool {
    matches!(
        std::env::var("TERMPDF_STRESS_RUN").ok().as_deref(),
        Some(v) if !v.is_empty() && v != "0"
    )
}

fn duration_secs() -> u64 {
    std::env::var("TERMPDF_STRESS_DURATION_SEC")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_DURATION_SEC)
}

fn binary_path() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for rel in ["target/release/termpdf", "target/debug/termpdf"] {
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
        .find(|p| Path::new(p).exists())?;
    let bindings = pdfium_render::prelude::Pdfium::bind_to_library(&lib).ok()?;
    Some(pdfium_render::prelude::Pdfium::new(bindings))
}

fn make_large_pdf(pdfium: &pdfium_render::prelude::Pdfium) -> PathBuf {
    use pdfium_render::prelude::*;
    let mut path = std::env::temp_dir();
    path.push(format!("termpdf-stress-{}.pdf", std::process::id()));
    let mut doc = pdfium.create_new_pdf().expect("create pdf");
    for i in 0..200 {
        let mut page = doc
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::a4())
            .expect("create page");
        for line in 0..12 {
            let text = format!(
                "page {} line {} the quick brown fox jumps over lazy dog",
                i + 1,
                line + 1
            );
            let font = doc.fonts_mut().helvetica();
            let mut object =
                PdfPageTextObject::new(&doc, text, font, PdfPoints::new(12.0)).expect("text");
            object
                .translate(
                    PdfPoints::new(50.0),
                    PdfPoints::new(750.0 - (line as f32 * 60.0)),
                )
                .expect("translate");
            page.objects_mut().add_text_object(object).expect("add");
        }
    }
    doc.save_to_file(&path).expect("save");
    path
}

#[cfg(target_os = "linux")]
fn read_rss_kb(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok());
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn read_rss_kb(_pid: u32) -> Option<u64> {
    None
}

fn count_transmits(haystack: &[u8]) -> usize {
    // Mirror the perf_harness counting logic: every `\x1b_G` followed
    // by `a=T` in the next 32 bytes is one transmit.
    let mut count = 0;
    let mut i = 0;
    while i + 4 < haystack.len() {
        if &haystack[i..i + 3] == b"\x1b_G" {
            let look_end = (i + 32).min(haystack.len());
            if haystack[i..look_end].windows(3).any(|w| w == b"a=T") {
                count += 1;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    count
}

#[test]
fn long_session_does_not_leak_or_runaway_transmit() {
    if !run_enabled() {
        eprintln!(
            "skipping: stress test gated behind TERMPDF_STRESS_RUN=1 \
             (set TERMPDF_STRESS_DURATION_SEC=N to override the {DEFAULT_DURATION_SEC}s default)"
        );
        return;
    }
    let Some(pdfium) = pdfium_or_skip() else {
        eprintln!("skipping: libpdfium not found");
        return;
    };
    let pdf = make_large_pdf(&pdfium);

    let bin = binary_path().expect("termpdf binary built");
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 960,
            pixel_height: 640,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(bin);
    cmd.arg(&pdf);
    cmd.arg("--protocol");
    cmd.arg("halfblocks");
    cmd.env("TERM", "xterm-kitty");
    cmd.env_remove("TMUX");
    cmd.env(
        "TERMPDF_PDFIUM",
        format!("{}/vendor/lib/libpdfium.so", env!("CARGO_MANIFEST_DIR")),
    );
    cmd.env("TERMPDF_CELL_PX", "8x16");
    let tmp_cache =
        std::env::temp_dir().join(format!("termpdf-stress-cache-{}", std::process::id()));
    cmd.env("XDG_CACHE_HOME", &tmp_cache);

    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    drop(pair.slave);
    let pid = child.process_id().expect("child pid");

    let mut reader = pair.master.try_clone_reader().expect("reader");
    let mut writer = pair.master.take_writer().expect("writer");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
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

    // Wait for first paint so the binary has actually started its
    // event loop. `1/200` is the page-indicator on a 200-page doc.
    let mut buf = Vec::new();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(8) {
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
            buf.extend_from_slice(&chunk);
            if buf.windows(5).any(|w| w == b"1/200") {
                break;
            }
        }
    }
    assert!(
        buf.windows(5).any(|w| w == b"1/200"),
        "binary never showed status `1/200` — startup hung"
    );

    let initial_rss = read_rss_kb(pid).unwrap_or(0);
    let mut peak_rss = initial_rss;
    let mut last_sample = Instant::now();
    let total_duration = Duration::from_secs(duration_secs());
    let scroll_started = Instant::now();
    let scroll_deadline = scroll_started + total_duration;

    let mut keystrokes = 0u64;
    while Instant::now() < scroll_deadline {
        // Burst-write 5 j's at a time so the binary's input throttle
        // sees a realistic held-key autorepeat pattern. 30 ms gap
        // between bursts roughly matches a fast reader's steady
        // cadence on a touch keyboard.
        for _ in 0..5 {
            let _ = writer.write_all(b"j");
            keystrokes += 1;
        }
        let _ = writer.flush();
        std::thread::sleep(Duration::from_millis(30));

        // Drain available output so the channel doesn't fill up.
        while let Ok(chunk) = rx.try_recv() {
            buf.extend_from_slice(&chunk);
        }

        if last_sample.elapsed() >= SAMPLE_INTERVAL {
            if let Some(rss) = read_rss_kb(pid) {
                if rss > peak_rss {
                    peak_rss = rss;
                }
            }
            last_sample = Instant::now();
        }
    }

    let scroll_elapsed = scroll_started.elapsed();
    let transmits = count_transmits(&buf);
    let transmits_per_sec = transmits as f64 / scroll_elapsed.as_secs_f64();
    let rss_ratio = if initial_rss > 0 {
        peak_rss as f64 / initial_rss as f64
    } else {
        f64::NAN
    };

    eprintln!(
        "stress: duration={:.1}s keystrokes={keystrokes} bytes_out={} \
         transmits={transmits} transmits/s={transmits_per_sec:.1} \
         initial_rss={initial_rss}KB peak_rss={peak_rss}KB ratio={rss_ratio:.2}×",
        scroll_elapsed.as_secs_f64(),
        buf.len(),
    );

    // Clean shutdown: send `qq` (visual-mode `q` exits, normal-mode
    // `q` quits) and wait for the child to exit. Same trick the pty
    // smoke test uses to dodge crossterm's Esc-coalesce ambiguity.
    let _ = writer.write_all(b"q");
    let _ = writer.flush();
    std::thread::sleep(Duration::from_millis(100));
    let _ = writer.write_all(b"q");
    let _ = writer.flush();

    let exit_deadline = Instant::now() + Duration::from_secs(5);
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

    // Cleanup the per-test cache dir.
    let _ = std::fs::remove_dir_all(&tmp_cache);
    let _ = std::fs::remove_file(&pdf);

    assert!(clean_exit, "binary did not exit within 5 s after qq");
    assert!(
        rss_ratio.is_nan() || rss_ratio < MAX_RSS_GROWTH_RATIO,
        "RSS grew {rss_ratio:.2}× over {:.0}s — leak suspect (initial={initial_rss}KB peak={peak_rss}KB)",
        scroll_elapsed.as_secs_f64()
    );
    assert!(
        transmits_per_sec < MAX_TRANSMITS_PER_SEC,
        "runaway transmits: {transmits_per_sec:.1}/s > {MAX_TRANSMITS_PER_SEC}/s — \
         points at a frame loop retransmitting every visible page"
    );
}
