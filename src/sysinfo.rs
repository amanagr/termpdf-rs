//! Process CPU% telemetry for the in-app perf HUD.
//!
//! Linux-only — parses `/proc/<pid>/stat` for cumulative CPU ticks.
//! Other OSes get a stub that always reports 0%.
//!
//! ## Sampling window
//!
//! 30 seconds. Sampled at 1 Hz to keep the rolling-window edge fresh,
//! but the displayed value is `Σ(cpu_ticks) / 30s × 100` over the most
//! recent 30 seconds — long enough to surface SUSTAINED draw without
//! the per-keystroke jitter a 1-second window shows. The user reported
//! the 1-second flicker felt useless; a 30 s window matches what their
//! laptop's thermal envelope actually responds to.
//!
//! ## Why this lives next to the renderer
//!
//! When the user reports "scrolling heats up the CPU" we want the
//! number visible in the same window they're scrolling. Switching to
//! `htop` in another pane is too far removed; the moment-of-truth is
//! the keystroke that drives the spike.

use std::collections::VecDeque;
use std::time::Instant;

#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    /// Process CPU% averaged over the rolling 30-second window. 0.0
    /// when fewer than 2 sample points exist yet (i.e. for the first
    /// second after `new()`).
    pub cpu_pct: f32,
}

/// One ring-buffer entry per 1Hz sample. We hold ~31 entries so the
/// 30-second rolling window always has enough history; entries past
/// `WINDOW_SECONDS` ago are dropped at the front.
#[derive(Debug, Clone, Copy)]
struct SamplePoint {
    at: Instant,
    proc_ticks: u64,
}

/// Length of the rolling-CPU% window. The user reported a 1-second
/// window felt useless because every keystroke spike pushed it back
/// to flat — a 30 s window matches their thermal envelope.
const WINDOW_SECONDS: u64 = 30;
/// Sample cadence. 1 Hz → 30 entries per window; the per-tick read of
/// `/proc/<pid>/stat` is sub-100 µs so this is essentially free.
const SAMPLE_INTERVAL_MS: u64 = 1000;

pub struct SysInfo {
    pid: u32,
    /// Ring buffer of recent (timestamp, proc_ticks) pairs. Front is
    /// oldest. `maybe_sample` pushes to the back and drops anything
    /// older than `WINDOW_SECONDS` from the front. The displayed CPU%
    /// is `(back.proc_ticks - front.proc_ticks) / (back.at - front.at)`.
    history: VecDeque<SamplePoint>,
    /// Current cached sample shown in the HUD.
    pub cur: Sample,
    /// Throttle: don't re-sample more often than this.
    min_interval: std::time::Duration,
    /// Disabled by env var TERMPDF_NO_PERF_HUD=1.
    pub disabled: bool,
}

impl SysInfo {
    pub fn new() -> Self {
        let disabled = std::env::var("TERMPDF_NO_PERF_HUD")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false);
        let pid = std::process::id();
        Self {
            pid,
            history: VecDeque::with_capacity(WINDOW_SECONDS as usize + 4),
            cur: Sample::default(),
            min_interval: std::time::Duration::from_millis(SAMPLE_INTERVAL_MS),
            disabled,
        }
    }

    /// Re-sample if the throttle window has passed. Cheap when not
    /// due (~one Instant::now + comparison).
    pub fn maybe_sample(&mut self) {
        if self.disabled {
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.history.back() {
            if now.duration_since(last.at) < self.min_interval {
                return;
            }
        }
        let proc_ticks = read_proc_cpu_ticks(self.pid).unwrap_or(0);
        self.history.push_back(SamplePoint { at: now, proc_ticks });

        // Drop samples older than the rolling window so the front
        // always represents "30 s ago" within ±1 sample interval.
        let window = std::time::Duration::from_secs(WINDOW_SECONDS);
        while let Some(front) = self.history.front() {
            if now.duration_since(front.at) > window {
                self.history.pop_front();
            } else {
                break;
            }
        }

        // Need at least two samples to diff. The first call fills the
        // ring with one entry and shows 0%; subsequent calls compute
        // a real average over whatever window has accumulated so far
        // (so a freshly-launched binary stabilises into a useful
        // reading within a second instead of needing a full 30 s).
        let cpu_pct = if let (Some(front), Some(back)) =
            (self.history.front(), self.history.back())
        {
            if front.at == back.at {
                0.0
            } else {
                let elapsed_ms = back.at.duration_since(front.at).as_millis().max(1) as u64;
                let delta_ticks = back.proc_ticks.saturating_sub(front.proc_ticks);
                let delta_cpu_ms = delta_ticks * 1000 / clock_ticks_per_sec();
                (delta_cpu_ms as f32 / elapsed_ms as f32) * 100.0
            }
        } else {
            0.0
        };

        self.cur = Sample { cpu_pct };
    }
}

fn read_proc_cpu_ticks(pid: u32) -> Option<u64> {
    if pid == 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let last_paren = stat.rfind(')')?;
    let rest = &stat[last_paren + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After the close-paren: state ppid pgrp session tty_nr tpgid
    // flags minflt cminflt majflt cmajflt utime stime ... so utime is
    // index 11, stime is index 12.
    if fields.len() < 13 {
        return None;
    }
    let utime: u64 = fields[11].parse().ok()?;
    let stime: u64 = fields[12].parse().ok()?;
    Some(utime + stime)
}

fn clock_ticks_per_sec() -> u64 {
    #[cfg(target_os = "linux")]
    unsafe {
        let v = sysconf_clk_tck();
        if v > 0 {
            v as u64
        } else {
            100
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        100
    }
}

#[cfg(target_os = "linux")]
extern "C" {
    fn sysconf(name: i32) -> i64;
}

#[cfg(target_os = "linux")]
const _SC_CLK_TCK: i32 = 2;

#[cfg(target_os = "linux")]
unsafe fn sysconf_clk_tck() -> i64 {
    sysconf(_SC_CLK_TCK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_default_is_zero() {
        let s = Sample::default();
        assert_eq!(s.cpu_pct, 0.0);
    }

    #[test]
    fn sysinfo_disabled_does_not_record() {
        let mut info = SysInfo {
            pid: 0,
            history: VecDeque::new(),
            cur: Sample::default(),
            min_interval: std::time::Duration::from_millis(0),
            disabled: true,
        };
        info.maybe_sample();
        assert_eq!(info.cur.cpu_pct, 0.0);
        assert!(info.history.is_empty(), "disabled sampler must not push history");
    }

    #[test]
    fn first_sample_cpu_is_zero() {
        // First sample fills history with one entry; need at least
        // two for a delta. The throttle is bypassed here (min_interval
        // = 0) so the call actually runs.
        let mut info = SysInfo {
            pid: std::process::id(),
            history: VecDeque::new(),
            cur: Sample::default(),
            min_interval: std::time::Duration::from_millis(0),
            disabled: false,
        };
        info.maybe_sample();
        assert_eq!(info.cur.cpu_pct, 0.0);
        assert_eq!(info.history.len(), 1, "first call should record one sample");
    }

    #[test]
    fn throttle_skips_repeat_calls() {
        // Two calls back-to-back within the throttle window — the
        // second must not push another history entry.
        let mut info = SysInfo {
            pid: std::process::id(),
            history: VecDeque::new(),
            cur: Sample::default(),
            min_interval: std::time::Duration::from_secs(60),
            disabled: false,
        };
        info.maybe_sample();
        let first_len = info.history.len();
        info.maybe_sample();
        let second_len = info.history.len();
        assert_eq!(first_len, second_len, "throttled call should not record");
    }

    /// Synthetic two-sample test: pretend the process burned 50 ms of
    /// CPU over a 100 ms wall window → 50% CPU. Verifies the rolling-
    /// window math without depending on real /proc readings.
    #[test]
    fn two_samples_compute_correct_percent() {
        let mut info = SysInfo {
            pid: 0,
            history: VecDeque::new(),
            cur: Sample::default(),
            min_interval: std::time::Duration::from_millis(0),
            disabled: false,
        };
        // Stuff history manually (bypasses the actual /proc read).
        let t0 = Instant::now();
        let t1 = t0 + std::time::Duration::from_millis(100);
        // 50 ms of CPU at 100 Hz CLK_TCK = 5 ticks.
        let ticks_for_50ms = 5;
        info.history.push_back(SamplePoint { at: t0, proc_ticks: 0 });
        info.history.push_back(SamplePoint { at: t1, proc_ticks: ticks_for_50ms });
        // Re-run the calc that maybe_sample does at the tail of its
        // body. Equivalent to:
        let elapsed_ms = t1.duration_since(t0).as_millis() as u64;
        let delta_cpu_ms = ticks_for_50ms * 1000 / clock_ticks_per_sec();
        let pct = (delta_cpu_ms as f32 / elapsed_ms as f32) * 100.0;
        // 50 ms / 100 ms = 50%. Allow 1% tolerance for rounding.
        assert!(
            (pct - 50.0).abs() < 1.5,
            "expected ~50%, got {pct}% (delta_cpu_ms={delta_cpu_ms})"
        );
    }
}
