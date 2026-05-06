//! Real-rendering performance harness.
//!
//! Spawns the built `termpdf` binary inside a PTY, drives scripted
//! action sequences against it, and captures wall-clock latency,
//! bytes-on-wire, transmit/placement counts, child-process CPU time,
//! and peak RSS for each scenario. Results are written to JSON; if a
//! baseline exists at `tests/perf_baseline.json`, the harness compares
//! and fails on regression beyond a per-metric tolerance.
//!
//! ## Why a separate harness instead of more `perf_regression.rs`
//!
//! `perf_regression.rs` is a smoke test — it asserts a few invariants
//! (transmits fire, disk cache writes happen). It doesn't measure
//! latency or compare across runs. This harness produces structured
//! per-scenario metrics that survive across commits, so the next
//! refactor that doubles selection-drag latency surfaces immediately
//! instead of at user-report time.
//!
//! ## What's measured
//!
//! Per scenario (an action sequence applied to a freshly-spawned
//! binary):
//!   - `wall_ms`: total elapsed real time from start of action sequence
//!     to last byte received from the child after the sequence completed.
//!   - `per_action_p50_ms` / `per_action_p95_ms`: distribution of
//!     per-keystroke response latency for sequences with multiple
//!     measured strokes (e.g. selection drag).
//!   - `bytes_out`: total bytes the child wrote during the scenario.
//!     Proxy for pty-write pressure (the lever that crashes Ghostty
//!     under load).
//!   - `transmits`: count of kitty `a=T` (image upload) escapes.
//!   - `placements`: count of kitty `a=p` (image placement) escapes.
//!   - `cpu_ms`: child utime+stime delta during the scenario, read
//!     from `/proc/<pid>/stat`. Linux-only; non-Linux runs report 0.
//!   - `peak_rss_kb`: max VmRSS sampled during the scenario from
//!     `/proc/<pid>/status`. Linux-only.
//!
//! ## Baseline / regression
//!
//! - First run on a machine: writes `tests/perf_baseline.json`.
//! - Subsequent runs: compare each metric against baseline; fail if
//!   any metric exceeds `1 + TERMPDF_PERF_TOLERANCE` (default 0.30
//!   = 30%) of baseline.
//! - `TERMPDF_PERF_UPDATE=1`: skip comparison, overwrite baseline
//!   with current run.
//! - `TERMPDF_PERF_VERBOSE=1`: print per-scenario detail.
//!
//! ## What this is NOT
//!
//! Not a benchmark for absolute performance comparisons across
//! machines — wall-clock numbers depend heavily on the host. The
//! harness reports relative drift against a per-machine baseline,
//! which is the actionable signal: "did this commit make X worse on
//! the same hardware".

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};

// ===========================================================================
// PDF fixtures
// ===========================================================================

/// A test corpus tier. Each scenario picks the smallest tier that
/// exercises its hot path so total test wall time stays bounded.
#[derive(Debug, Clone, Copy)]
enum FixtureSize {
    /// 5 pages, sparse text. Used by open / dark / single-page tests.
    Tiny,
    /// 50 pages, ~1 KB text per page. Used by scroll / search tests.
    Medium,
    /// 200 pages, ~2 KB text per page. Used by big-jump tests where
    /// the document needs enough pages that a 100G overshoots most
    /// already-cached prefixes.
    Large,
}

impl FixtureSize {
    fn pages(self) -> usize {
        match self {
            FixtureSize::Tiny => 5,
            FixtureSize::Medium => 50,
            FixtureSize::Large => 200,
        }
    }
}

/// Build a synthetic PDF using pdfium-render. Returns the path on
/// disk. Caller is responsible for cleanup.
fn make_test_pdf(
    pdfium: &pdfium_render::prelude::Pdfium,
    size: FixtureSize,
    label: &str,
) -> PathBuf {
    use pdfium_render::prelude::*;
    let mut path = std::env::temp_dir();
    path.push(format!(
        "termpdf-perf-{}-{}-{}.pdf",
        label,
        size.pages(),
        std::process::id()
    ));
    let mut doc = pdfium.create_new_pdf().expect("create pdf");
    let n_pages = size.pages();
    for i in 0..n_pages {
        let mut page = doc
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::a4())
            .expect("create page");
        // Multiple text objects per page so a Visual-mode selection
        // drag has chars to traverse and search has hits to find.
        // Spread vertically so line motions land somewhere meaningful.
        let lines_per_page = match size {
            FixtureSize::Tiny => 4,
            FixtureSize::Medium => 8,
            FixtureSize::Large => 12,
        };
        for line in 0..lines_per_page {
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

// ===========================================================================
// Pdfium binding
// ===========================================================================

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
        .find(|p| Path::new(p).exists());
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

// ===========================================================================
// PTY driver
// ===========================================================================

struct Pty {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<Vec<u8>>,
    pid: u32,
    /// Wallclock at spawn — referenced by metrics that want to know
    /// "how far into the run did event X happen".
    spawned_at: Instant,
}

impl Pty {
    fn spawn(pdf: &Path, scratch_cache: &Path) -> Self {
        let bin = binary_path().expect("binary built — run cargo build --release first");
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 800,
                pixel_height: 600,
            })
            .expect("openpty");

        let mut cmd = CommandBuilder::new(&bin);
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
        cmd.env("XDG_CACHE_HOME", scratch_cache);

        let child = pair.slave.spawn_command(cmd).expect("spawn");
        let pid = child.process_id().unwrap_or(0);
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("reader");
        let writer = pair.master.take_writer().expect("writer");

        let (tx, rx) = mpsc::channel();
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

        Pty {
            child,
            writer,
            rx,
            pid,
            spawned_at: Instant::now(),
        }
    }

    /// Send keystrokes to the child. Bytes are written verbatim — use
    /// `\r` for Enter, `\x1b` for Esc, etc.
    fn send(&mut self, keys: &[u8]) {
        let _ = self.writer.write_all(keys);
        let _ = self.writer.flush();
    }

    /// Drain output for at most `max_wait`, returning when no byte
    /// has arrived for `quiet`. The classic "wait for the frame to
    /// settle" pattern. Returns the bytes drained during this call.
    fn drain_quiet(&self, quiet: Duration, max_wait: Duration) -> Vec<u8> {
        let mut out = Vec::new();
        let deadline = Instant::now() + max_wait;
        let mut last_byte = Instant::now();
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            if !out.is_empty() && now.duration_since(last_byte) >= quiet {
                break;
            }
            // Smaller chunks for tighter quiet detection. recv_timeout
            // wakes on either a chunk arrival OR the timeout, whichever
            // first.
            let remaining = deadline.saturating_duration_since(now);
            let wait = remaining.min(quiet.min(Duration::from_millis(20)));
            match self.rx.recv_timeout(wait) {
                Ok(chunk) => {
                    last_byte = Instant::now();
                    out.extend_from_slice(&chunk);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Fall through; loop checks quiet condition.
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        out
    }

    /// Read `/proc/<pid>/stat` and return (utime + stime) in clock
    /// ticks. Linux-only. Returns 0 on read failure or non-Linux.
    fn proc_cpu_ticks(&self) -> u64 {
        if self.pid == 0 {
            return 0;
        }
        let stat = match std::fs::read_to_string(format!("/proc/{}/stat", self.pid)) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        // /proc/<pid>/stat format: pid (comm) state ppid pgrp ... utime stime ...
        // The (comm) field can contain spaces and parens. Find the LAST `)` and
        // tokenize from there to avoid the trap.
        let last_paren = match stat.rfind(')') {
            Some(i) => i,
            None => return 0,
        };
        let rest = &stat[last_paren + 1..];
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // After the close-paren, fields are: state ppid pgrp session tty_nr
        // tpgid flags minflt cminflt majflt cmajflt utime stime ... so utime is
        // index 11, stime is index 12 (0-indexed) of `fields`.
        if fields.len() < 13 {
            return 0;
        }
        let utime: u64 = fields[11].parse().unwrap_or(0);
        let stime: u64 = fields[12].parse().unwrap_or(0);
        utime + stime
    }

    /// Read `/proc/<pid>/status` VmRSS in KB. Linux-only.
    fn proc_rss_kb(&self) -> u64 {
        if self.pid == 0 {
            return 0;
        }
        let status = match std::fs::read_to_string(format!("/proc/{}/status", self.pid)) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0);
                return kb;
            }
        }
        0
    }

    /// Quit the child gracefully. Best-effort; falls through to kill
    /// after a timeout.
    fn quit(mut self) -> bool {
        let _ = self.writer.write_all(b":q\r");
        let _ = self.writer.flush();
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        let _ = self.child.kill();
        false
    }
}

// Linux clock-tick conversion. `getconf CLK_TCK` is 100 on every Linux
// I've used; falls back to 100 if the conversion fails.
fn clock_ticks_per_sec() -> u64 {
    // SAFETY: sysconf is a pure read; no thread safety concerns.
    let v = unsafe { libc_sysconf_clk_tck() };
    if v <= 0 {
        100
    } else {
        v as u64
    }
}

#[cfg(target_os = "linux")]
extern "C" {
    fn sysconf(name: i32) -> i64;
}

#[cfg(target_os = "linux")]
const _SC_CLK_TCK: i32 = 2;

#[cfg(target_os = "linux")]
unsafe fn libc_sysconf_clk_tck() -> i64 {
    sysconf(_SC_CLK_TCK)
}

#[cfg(not(target_os = "linux"))]
unsafe fn libc_sysconf_clk_tck() -> i64 {
    100
}

// ===========================================================================
// Output parsing
// ===========================================================================

/// Count occurrences of `\x1b_Gq=1,...,a=T,...` (transmit) and
/// `\x1b_Ga=p,...` (placement) escapes in a byte stream. Both forms
/// also appear wrapped in tmux-passthrough DCS, but we run the binary
/// with TMUX env removed so the unwrapped form is what hits the wire.
fn count_escapes(haystack: &[u8]) -> (usize, usize) {
    let mut transmits = 0;
    let mut placements = 0;
    let mut i = 0;
    while i + 4 < haystack.len() {
        if &haystack[i..i + 3] == b"\x1b_G" {
            // Look at the next ~16 bytes to classify. Both T and p
            // appear within the first parameter token.
            let look_end = (i + 32).min(haystack.len());
            let slice = &haystack[i..look_end];
            // The first ~16 bytes contain a sequence of `key=value,`
            // params. Find `a=T` for transmit, `a=p` for placement.
            // Both forms are exact, so byte-level windowing is fine.
            if window_contains(slice, b"a=T") {
                transmits += 1;
            } else if window_contains(slice, b"a=p") {
                placements += 1;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    (transmits, placements)
}

fn window_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// True if the byte stream contains the page-indicator status line
/// fragment `<n>/<total>` for a known total (signals first paint
/// committed).
fn has_page_indicator(buf: &[u8], total: usize) -> bool {
    let needle = format!("/{}", total);
    buf.windows(needle.len()).any(|w| w == needle.as_bytes())
}

// ===========================================================================
// Metric model
// ===========================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Metric {
    wall_ms: u64,
    per_action_p50_ms: u64,
    per_action_p95_ms: u64,
    bytes_out: u64,
    transmits: u64,
    placements: u64,
    cpu_ms: u64,
    peak_rss_kb: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BaselineFile {
    /// Bumped when scenarios change shape so a stale baseline doesn't
    /// silently mis-compare across schema revisions.
    version: u32,
    /// Free-form string the harness writer can stamp; not consulted.
    note: String,
    scenarios: BTreeMap<String, Metric>,
}

const BASELINE_VERSION: u32 = 1;

// ===========================================================================
// Scenario primitives
// ===========================================================================

/// One unit of action sequence. Each variant maps to a pty.send call
/// followed by a drain — the harness measures wall_ms across the
/// drain and aggregates over all Action steps in a scenario.
#[derive(Debug, Clone)]
enum Action {
    /// Send raw bytes. Drain quiet up to max_wait.
    Keys(&'static [u8], Duration),
    /// Send the same keystroke `n` times, draining after each. Used
    /// for sequences where we want per-action latency stats.
    Repeat {
        keys: &'static [u8],
        n: usize,
        max_wait: Duration,
    },
    /// Sleep for the given duration without sending keys. Lets idle
    /// prefetch fire (or the binary settle) before the next action.
    Idle(Duration),
}

#[derive(Debug, Clone)]
struct Scenario {
    name: &'static str,
    fixture: FixtureSize,
    /// Sent before the measured part — e.g. waiting for first paint
    /// or registering one input so idle prefetch isn't `Skip`.
    setup: Vec<Action>,
    /// The measured part. Per-action latencies aggregate here.
    measured: Vec<Action>,
}

// ===========================================================================
// Scenario catalog
// ===========================================================================

fn scenarios() -> Vec<Scenario> {
    use Action::*;
    let q = Duration::from_millis(50); // quiet threshold
    let _ = q; // currently unused at scenario decl; quiet lives in run_scenario
    vec![
        // Cold open: spawn → first paint. The setup is empty and we
        // measure the time until the page indicator appears in the
        // status line. Captures pdfium binding init + first render +
        // first transmit.
        Scenario {
            name: "open_tiny",
            fixture: FixtureSize::Tiny,
            setup: vec![],
            measured: vec![
                // Special "no-op" key just to anchor the measurement;
                // we're really measuring the spawn → indicator path
                // in run_scenario's wait_for_first_paint helper.
                Idle(Duration::from_millis(10)),
            ],
        },
        Scenario {
            name: "open_medium",
            fixture: FixtureSize::Medium,
            setup: vec![],
            measured: vec![Idle(Duration::from_millis(10))],
        },
        // Steady scroll: 30 sequential `j`s on a 50-page doc. Most
        // pages already cached after the first ~5; subsequent pages
        // either hit page_cache or get cold-rendered one per draw.
        // This is the bread-and-butter user motion.
        Scenario {
            name: "scroll_steady_30",
            fixture: FixtureSize::Medium,
            setup: vec![Idle(Duration::from_millis(300))],
            measured: vec![Repeat {
                keys: b"j",
                n: 30,
                max_wait: Duration::from_millis(400),
            }],
        },
        // Big jump: G to last page on a 200-page doc. Stresses cold-
        // render staggering + transmit budget — the bug class that
        // crashed Ghostty before we capped per-frame transmits.
        Scenario {
            name: "jump_to_end_large",
            fixture: FixtureSize::Large,
            setup: vec![Idle(Duration::from_millis(300))],
            measured: vec![Keys(b"G", Duration::from_millis(800))],
        },
        // Dark toggle: layout key change. Clears page_cache, forces
        // re-render of every visible page. The path that would burn
        // CPU spectacularly without idle-tier policy.
        Scenario {
            name: "dark_toggle",
            fixture: FixtureSize::Medium,
            setup: vec![Idle(Duration::from_millis(300))],
            measured: vec![Keys(b":set dark\r", Duration::from_millis(800))],
        },
        // Search: build the index (already pre-warmed during idle)
        // then advance through 5 hits.
        Scenario {
            name: "search_advance_5",
            fixture: FixtureSize::Medium,
            setup: vec![
                // Wait long enough for idle to fully index a 50-page
                // doc — text-index is the IDLE_TEXT_INDEX_MS tier so
                // we need >1s of idle, plus N*250ms ticks for the
                // pages.
                Idle(Duration::from_millis(2500)),
                Keys(b"/the\r", Duration::from_millis(400)),
            ],
            measured: vec![Repeat {
                keys: b"n",
                n: 5,
                max_wait: Duration::from_millis(400),
            }],
        },
        // Visual-mode selection drag: enter Visual, send 30 `l`s.
        // THE hot path — every keystroke currently re-encodes the
        // active page's full bitmap. This scenario is the canary
        // for the selection-overlay-as-separate-image work.
        Scenario {
            name: "selection_drag_30",
            fixture: FixtureSize::Medium,
            setup: vec![
                Idle(Duration::from_millis(300)),
                // Enter Visual placement, then commit to selection
                // mode so each subsequent l grows the band (not
                // just moves the caret in placement mode).
                Keys(b"v", Duration::from_millis(200)),
                Keys(b"v", Duration::from_millis(100)),
            ],
            measured: vec![Repeat {
                keys: b"l",
                n: 30,
                max_wait: Duration::from_millis(400),
            }],
        },
        // Highlight add: visual-select 10 chars + y. Fires
        // highlight_revision++ → all visible pages stale →
        // re-bake + re-encode + re-transmit cascade.
        Scenario {
            name: "highlight_add",
            fixture: FixtureSize::Medium,
            setup: vec![
                Idle(Duration::from_millis(300)),
                Keys(b"v", Duration::from_millis(150)),
                Keys(b"v", Duration::from_millis(100)),
                // 10 char extension.
                Repeat {
                    keys: b"l",
                    n: 10,
                    max_wait: Duration::from_millis(200),
                },
            ],
            measured: vec![Keys(b"y", Duration::from_millis(600))],
        },
        // Zoom in then out. Each zoom invalidates layout → clears
        // caches → re-renders visible pages.
        Scenario {
            name: "zoom_cycle",
            fixture: FixtureSize::Medium,
            setup: vec![Idle(Duration::from_millis(300))],
            measured: vec![
                Keys(b"+", Duration::from_millis(600)),
                Keys(b"-", Duration::from_millis(600)),
            ],
        },
        // Long idle after open on a large doc — surfaces the steady-
        // state CPU cost. With the prefetch window full, idle warm
        // falls through to text indexing.
        Scenario {
            name: "idle_after_open_large",
            fixture: FixtureSize::Large,
            setup: vec![
                Keys(b"g", Duration::from_millis(100)),
                Idle(Duration::from_millis(300)),
            ],
            measured: vec![Idle(Duration::from_secs(5))],
        },
        // Idle AFTER settling — proxy for "Ghostty CPU when user does
        // nothing." Setup spends 4 s draining the post-input transmit
        // burst (idle warm + Sharp upgrade re-transmits); the
        // measured window is the next 3 s, which should be near-silent
        // on the pty. If anything keeps firing here — a 60 Hz draw
        // loop, an unguarded periodic redraw, an idle warm path that
        // re-transmits forever — bytes_out spikes and the test
        // catches it. Set the baseline tight so a regression is
        // visible. The user reported a sustained 50-90% Ghostty CPU
        // during exactly this window before we gated term.draw on a
        // `dirty` flag.
        Scenario {
            name: "idle_after_settle_quiet",
            fixture: FixtureSize::Large,
            setup: vec![
                Keys(b"g", Duration::from_millis(100)),
                Keys(b"j", Duration::from_millis(150)),
                // Long enough that warm prefetch + Sharp upgrade
                // settle. 4 s clears all visible-page upgrades on
                // the Large fixture (50 pages, ~5 visible).
                Idle(Duration::from_secs(4)),
            ],
            measured: vec![Idle(Duration::from_secs(3))],
        },
        // Casual scroll on a large book — what the user reported.
        // Each j press lands on an uncached page; the per-scroll
        // cost path runs end-to-end (pdfium render + bake +
        // PNG encode + transmit + disk cache write). 30 scrolls
        // simulates ~10 seconds of page flipping at user pace; the
        // cpu_ms column is the heat canary.
        Scenario {
            name: "scroll_casual_large",
            fixture: FixtureSize::Large,
            setup: vec![
                Keys(b"g", Duration::from_millis(100)),
                Keys(b"k", Duration::from_millis(150)),
                Idle(Duration::from_millis(300)),
            ],
            measured: vec![Repeat {
                keys: b"j",
                n: 30,
                max_wait: Duration::from_millis(500),
            }],
        },
    ]
}

// ===========================================================================
// Scenario execution
// ===========================================================================

/// Number of times each scenario runs; the median of each metric is
/// reported as the final result. PTY-based wall-clock measurements are
/// noisy on a shared machine (background load, scheduler quantization,
/// cache temperatures); a single run can be 1.5× the median for purely
/// environmental reasons. Three runs per scenario costs ~50 s of test
/// wall but gets the regression detector out of "every run is a
/// regression on the last" purgatory.
const RUNS_PER_SCENARIO: usize = 3;

/// Median of a slice of metrics, field by field. The metrics come
/// out of N runs of the same scenario; treating each metric
/// independently means the "median run" is synthetic — its wall_ms
/// might come from run 2 while its bytes_out comes from run 1. That's
/// the right behaviour: bytes_out is deterministic and shouldn't be
/// dragged around by the noisy wall-clock fields.
fn median_metric(samples: &[Metric]) -> Metric {
    fn med(field: impl Fn(&Metric) -> u64, samples: &[Metric]) -> u64 {
        let mut v: Vec<u64> = samples.iter().map(field).collect();
        v.sort_unstable();
        v[v.len() / 2]
    }
    Metric {
        wall_ms: med(|m| m.wall_ms, samples),
        per_action_p50_ms: med(|m| m.per_action_p50_ms, samples),
        per_action_p95_ms: med(|m| m.per_action_p95_ms, samples),
        bytes_out: med(|m| m.bytes_out, samples),
        transmits: med(|m| m.transmits, samples),
        placements: med(|m| m.placements, samples),
        cpu_ms: med(|m| m.cpu_ms, samples),
        peak_rss_kb: med(|m| m.peak_rss_kb, samples),
    }
}

/// Run a single scenario and return its metric. Panics on internal
/// pty / spawn failure (test would fail anyway).
fn run_scenario_once(pdfium: &pdfium_render::prelude::Pdfium, scenario: &Scenario) -> Metric {
    let pdf = make_test_pdf(pdfium, scenario.fixture, scenario.name);
    let scratch_cache = std::env::temp_dir().join(format!(
        "perf-harness-{}-{}",
        scenario.name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&scratch_cache);
    std::fs::create_dir_all(&scratch_cache).expect("scratch cache");

    let mut pty = Pty::spawn(&pdf, &scratch_cache);

    // Wait for first paint (page indicator in the status line). This
    // is the anchor for "open" scenarios and a precondition for any
    // measured action — without first paint the binary may still be
    // in startup and our keystrokes get dropped.
    let total = scenario.fixture.pages();
    let first_paint_ms = wait_for_first_paint(&pty, total);

    let mut bytes_out: u64 = 0;
    let mut transmits: u64 = 0;
    let mut placements: u64 = 0;
    let mut peak_rss_kb: u64 = pty.proc_rss_kb();
    let cpu_ticks_start = pty.proc_cpu_ticks();

    // Setup phase — unmeasured per-action, but bytes/transmits
    // captured for accounting (so peak_rss_kb tracks setup too).
    for action in &scenario.setup {
        execute_action(
            &mut pty,
            action,
            &mut bytes_out,
            &mut transmits,
            &mut placements,
            &mut peak_rss_kb,
            None,
        );
    }

    // Measured phase. Per-action latencies collected for Repeat.
    let measured_start = Instant::now();
    let mut per_action_ms: Vec<u64> = Vec::new();

    for action in &scenario.measured {
        execute_action(
            &mut pty,
            action,
            &mut bytes_out,
            &mut transmits,
            &mut placements,
            &mut peak_rss_kb,
            Some(&mut per_action_ms),
        );
    }

    let measured_wall = measured_start.elapsed();
    let cpu_ticks_end = pty.proc_cpu_ticks();
    peak_rss_kb = peak_rss_kb.max(pty.proc_rss_kb());

    let _ = pty.quit();

    let cpu_ms = if cpu_ticks_end > cpu_ticks_start {
        let ticks = cpu_ticks_end - cpu_ticks_start;
        ticks * 1000 / clock_ticks_per_sec()
    } else {
        0
    };

    let (p50, p95) = if per_action_ms.is_empty() {
        // Single-shot scenarios: report the whole measured phase as
        // p50/p95 so the per-action columns are meaningful.
        let v = measured_wall.as_millis() as u64;
        (v, v)
    } else {
        percentiles(&per_action_ms)
    };

    let _ = std::fs::remove_dir_all(&scratch_cache);
    let _ = std::fs::remove_file(&pdf);

    let wall = if scenario.name.starts_with("open_") {
        // Open scenarios: report time-to-first-paint instead of the
        // post-paint Idle drain.
        first_paint_ms
    } else {
        measured_wall.as_millis() as u64
    };

    Metric {
        wall_ms: wall,
        per_action_p50_ms: p50,
        per_action_p95_ms: p95,
        bytes_out,
        transmits,
        placements,
        cpu_ms,
        peak_rss_kb,
    }
}

fn execute_action(
    pty: &mut Pty,
    action: &Action,
    bytes_out: &mut u64,
    transmits: &mut u64,
    placements: &mut u64,
    peak_rss: &mut u64,
    mut per_action_ms: Option<&mut Vec<u64>>,
) {
    match action {
        Action::Keys(bytes, max_wait) => {
            let t0 = Instant::now();
            pty.send(bytes);
            let drained = pty.drain_quiet(Duration::from_millis(50), *max_wait);
            *bytes_out += drained.len() as u64;
            let (t, p) = count_escapes(&drained);
            *transmits += t as u64;
            *placements += p as u64;
            *peak_rss = (*peak_rss).max(pty.proc_rss_kb());
            if let Some(v) = per_action_ms.as_mut() {
                v.push(t0.elapsed().as_millis() as u64);
            }
        }
        Action::Repeat { keys, n, max_wait } => {
            for _ in 0..*n {
                let t0 = Instant::now();
                pty.send(keys);
                let drained = pty.drain_quiet(Duration::from_millis(40), *max_wait);
                *bytes_out += drained.len() as u64;
                let (t, p) = count_escapes(&drained);
                *transmits += t as u64;
                *placements += p as u64;
                if let Some(v) = per_action_ms.as_mut() {
                    v.push(t0.elapsed().as_millis() as u64);
                }
            }
            *peak_rss = (*peak_rss).max(pty.proc_rss_kb());
        }
        Action::Idle(d) => {
            // Idle drains output but doesn't send. Captures any
            // pre-transmits the idle warm path emits.
            let drained = pty.drain_quiet(Duration::from_millis(40), *d);
            *bytes_out += drained.len() as u64;
            let (t, p) = count_escapes(&drained);
            *transmits += t as u64;
            *placements += p as u64;
            *peak_rss = (*peak_rss).max(pty.proc_rss_kb());
        }
    }
}

/// Watch the drained output until the page indicator (`<n>/<total>`)
/// appears, signalling first paint. Returns ms elapsed from the
/// pty's spawn instant. Falls back to the timeout value on miss.
fn wait_for_first_paint(pty: &Pty, total_pages: usize) -> u64 {
    let mut buf: Vec<u8> = Vec::new();
    let max = Duration::from_secs(8);
    let deadline = Instant::now() + max;
    while Instant::now() < deadline {
        match pty.rx.recv_timeout(
            Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
        ) {
            Ok(chunk) => {
                buf.extend_from_slice(&chunk);
                if has_page_indicator(&buf, total_pages) {
                    return pty.spawned_at.elapsed().as_millis() as u64;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if has_page_indicator(&buf, total_pages) {
                    return pty.spawned_at.elapsed().as_millis() as u64;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    max.as_millis() as u64
}

fn percentiles(samples: &[u64]) -> (u64, u64) {
    if samples.is_empty() {
        return (0, 0);
    }
    let mut sorted: Vec<u64> = samples.to_vec();
    sorted.sort_unstable();
    let p50_idx = (sorted.len() - 1) / 2;
    let p95_idx = ((sorted.len() as f64 * 0.95) as usize).min(sorted.len() - 1);
    (sorted[p50_idx], sorted[p95_idx])
}

// ===========================================================================
// Baseline persistence
// ===========================================================================

fn baseline_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("perf_baseline.json")
}

fn load_baseline() -> Option<BaselineFile> {
    let p = baseline_path();
    let s = std::fs::read_to_string(&p).ok()?;
    let mut bf: BaselineFile = serde_json::from_str(&s).ok()?;
    if bf.version != BASELINE_VERSION {
        eprintln!(
            "perf-harness: baseline version mismatch (file={}, expected={}), \
             ignoring baseline",
            bf.version, BASELINE_VERSION
        );
        return None;
    }
    // Belt-and-braces: ensure scenarios is a fresh BTreeMap rather
    // than whatever serde_json produced (no-op functionally; helps
    // determinism if BTreeMap's serde impl ever changes).
    let scenarios = std::mem::take(&mut bf.scenarios);
    bf.scenarios = scenarios;
    Some(bf)
}

fn save_baseline(bf: &BaselineFile) {
    let p = baseline_path();
    if let Ok(s) = serde_json::to_string_pretty(bf) {
        let _ = std::fs::write(&p, s);
    }
}

fn tolerance() -> f64 {
    std::env::var("TERMPDF_PERF_TOLERANCE")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.30)
}

fn verbose() -> bool {
    std::env::var("TERMPDF_PERF_VERBOSE")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

fn update_mode() -> bool {
    std::env::var("TERMPDF_PERF_UPDATE")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

// ===========================================================================
// Comparison logic
// ===========================================================================

/// Compare actual vs baseline. Returns Vec<(scenario, metric, ratio)>
/// of regressions exceeding the tolerance.
fn compare(
    actual: &BTreeMap<String, Metric>,
    baseline: &BTreeMap<String, Metric>,
    tol: f64,
) -> Vec<(String, &'static str, f64, u64, u64)> {
    let mut regressions = Vec::new();
    for (name, a) in actual {
        let b = match baseline.get(name) {
            Some(b) => b,
            None => continue, // new scenario, no baseline to compare
        };
        for (label, av, bv) in [
            ("wall_ms", a.wall_ms, b.wall_ms),
            (
                "per_action_p50_ms",
                a.per_action_p50_ms,
                b.per_action_p50_ms,
            ),
            (
                "per_action_p95_ms",
                a.per_action_p95_ms,
                b.per_action_p95_ms,
            ),
            ("bytes_out", a.bytes_out, b.bytes_out),
            ("transmits", a.transmits, b.transmits),
            ("cpu_ms", a.cpu_ms, b.cpu_ms),
            ("peak_rss_kb", a.peak_rss_kb, b.peak_rss_kb),
        ] {
            // Skip metrics where the baseline is too small to make
            // ratios meaningful (a 5 ms baseline that grew to 20 ms is
            // 4× — but 15 ms of variance is jitter, not regression).
            // The floor is per-metric specific.
            let floor = match label {
                "wall_ms" | "per_action_p50_ms" | "per_action_p95_ms" | "cpu_ms" => 30,
                "bytes_out" => 4096,
                "transmits" | "placements" => 1,
                "peak_rss_kb" => 4096,
                _ => 1,
            };
            if bv < floor {
                continue;
            }
            let ratio = av as f64 / bv as f64;
            if ratio > 1.0 + tol {
                regressions.push((name.clone(), label, ratio, av, bv));
            }
        }
    }
    regressions
}

// ===========================================================================
// Test entry point
// ===========================================================================

#[test]
fn perf_baseline_or_regression() {
    let Some(pdfium) = pdfium_or_skip() else {
        return;
    };
    let scenarios = scenarios();
    let mut actual = BTreeMap::new();

    println!(
        "perf-harness: running {} scenarios × {} runs each",
        scenarios.len(),
        RUNS_PER_SCENARIO
    );
    let total_t0 = Instant::now();
    for (i, s) in scenarios.iter().enumerate() {
        let t0 = Instant::now();
        let mut samples: Vec<Metric> = Vec::with_capacity(RUNS_PER_SCENARIO);
        for _ in 0..RUNS_PER_SCENARIO {
            samples.push(run_scenario_once(&pdfium, s));
        }
        let m = median_metric(&samples);
        if verbose() {
            println!(
                "  [{:>2}/{}] {:<22} wall={:>5}ms p50={:>4}ms p95={:>4}ms bytes={:>7} \
                 transmits={:>3} placements={:>4} cpu={:>4}ms rss={:>6}KB ({}ms)",
                i + 1,
                scenarios.len(),
                s.name,
                m.wall_ms,
                m.per_action_p50_ms,
                m.per_action_p95_ms,
                m.bytes_out,
                m.transmits,
                m.placements,
                m.cpu_ms,
                m.peak_rss_kb,
                t0.elapsed().as_millis()
            );
        } else {
            println!("  {:<22} {:>5}ms", s.name, m.wall_ms);
        }
        actual.insert(s.name.to_string(), m);
    }
    println!(
        "perf-harness: {} scenarios in {:.1}s",
        scenarios.len(),
        total_t0.elapsed().as_secs_f64()
    );

    let bf_actual = BaselineFile {
        version: BASELINE_VERSION,
        note: format!(
            "generated {}",
            chrono_like_timestamp_or(String::from("(no timestamp)"))
        ),
        scenarios: actual.clone(),
    };

    if update_mode() {
        save_baseline(&bf_actual);
        println!("perf-harness: TERMPDF_PERF_UPDATE=1 — baseline written, no comparison");
        return;
    }

    let baseline = match load_baseline() {
        Some(b) => b,
        None => {
            println!(
                "perf-harness: no baseline file at {} — writing current run as baseline",
                baseline_path().display()
            );
            save_baseline(&bf_actual);
            return;
        }
    };

    let tol = tolerance();
    let regressions = compare(&actual, &baseline.scenarios, tol);
    if regressions.is_empty() {
        println!(
            "perf-harness: no regressions vs baseline (tolerance ±{:.0}%)",
            tol * 100.0
        );
        return;
    }

    eprintln!(
        "perf-harness: {} regression(s) exceeding ±{:.0}% tolerance:",
        regressions.len(),
        tol * 100.0
    );
    for (scn, metric, ratio, av, bv) in &regressions {
        eprintln!(
            "  {} :: {} = {} (was {}, ratio {:.2}×)",
            scn, metric, av, bv, ratio
        );
    }
    panic!(
        "perf regression detected — re-run with TERMPDF_PERF_VERBOSE=1 for context, \
         or TERMPDF_PERF_UPDATE=1 to accept new baseline"
    );
}

/// Stand-in timestamp helper. The harness writes it into the baseline
/// `note` so a human grepping the file knows when the baseline was
/// captured. Best-effort; uses SystemTime since std::time doesn't
/// give us a calendar date without an extra dep.
fn chrono_like_timestamp_or(fallback: String) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| format!("unix={}", d.as_secs()))
        .unwrap_or(fallback)
}
