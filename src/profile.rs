//! Optional per-frame phase timing. Enable with `TERMPDF_PROFILE=1`.
//!
//! Prints a per-phase summary (n / mean / p50 / p95 / max in microseconds)
//! to stderr after the alternate screen is torn down. Disabled by default;
//! when disabled, every `span()` returns `None` and the Drop is a no-op,
//! so there's no measurable overhead in normal runs.
//!
//! Use case: hold `j` for ~5 seconds in a real PDF, quit, read the
//! summary. The phase whose `mean` × `n` dominates is the bottleneck.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Instant;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Synchronous pdfium rasterise of any visible page that wasn't
    /// already cached. The fast scroll case has this at zero work.
    EnsureRendered,
    /// Bake highlights+selection into a per-page overlay bitmap.
    EnsureOverlay,
    /// Whichever compose path won (scroll-shift / selection-only /
    /// full rebuild). Wraps the memmove + strip repaint.
    Compose,
    /// `build_protocol`: `ImageSource::new` (hashes the canvas inside
    /// ratatui-image) + `StatefulProtocol::new`.
    BuildProtocol,
    /// `terminal.draw(...)`. Includes ratatui-image's base64 encode
    /// of the canvas and the actual write to the pty. This is the
    /// suspected steady-scroll bottleneck.
    Draw,
}

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("TERMPDF_PROFILE")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    })
}

thread_local! {
    static SAMPLES: RefCell<HashMap<Phase, Vec<u64>>> =
        RefCell::new(HashMap::new());
}

pub struct Span {
    phase: Phase,
    start: Instant,
}

#[must_use]
pub fn span(phase: Phase) -> Option<Span> {
    enabled().then(|| Span { phase, start: Instant::now() })
}

impl Drop for Span {
    fn drop(&mut self) {
        let us = self.start.elapsed().as_micros() as u64;
        let _ = SAMPLES.try_with(|s| {
            s.borrow_mut().entry(self.phase).or_default().push(us);
        });
    }
}

pub fn report() {
    if !enabled() {
        return;
    }
    SAMPLES.with(|s| {
        let mut samples = s.borrow_mut();
        let mut entries: Vec<_> = samples.iter_mut().collect();
        entries.sort_by_key(|(p, _)| format!("{p:?}"));
        eprintln!();
        eprintln!("=== termpdf phase timings (microseconds) ===");
        eprintln!(
            "{:<16} {:>6} {:>10} {:>10} {:>10} {:>10} {:>10}",
            "phase", "n", "mean", "p50", "p95", "max", "total_ms"
        );
        for (phase, times) in entries {
            if times.is_empty() {
                continue;
            }
            times.sort_unstable();
            let n = times.len() as u64;
            let sum: u64 = times.iter().sum();
            let mean = sum / n;
            let p50 = times[times.len() / 2];
            let p95 = times[(times.len() * 95 / 100).min(times.len() - 1)];
            let max = *times.last().unwrap();
            let total_ms = sum / 1000;
            eprintln!(
                "{:<16} {:>6} {:>10} {:>10} {:>10} {:>10} {:>10}",
                format!("{phase:?}"),
                n,
                mean,
                p50,
                p95,
                max,
                total_ms
            );
        }
    });
}
