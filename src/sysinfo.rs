//! Process + system telemetry for the in-app perf HUD.
//!
//! Sampled at 1 Hz from the run-loop tick; the status line reads the
//! cached values so the read path is allocation-free. Linux-only (we
//! parse `/proc/<pid>/stat`, `/proc/<pid>/status`, and walk
//! `/sys/class/hwmon` for CPU temperature). Other OSes get a stub
//! sampler that always reports None.
//!
//! ## CPU%
//!
//! Computed as `(delta utime+stime) / (delta wall) * 100`. The delta
//! is captured between successive `sample()` calls; first call after
//! `new()` returns 0% because there's no prior ticks to diff against.
//!
//! ## Power
//!
//! Walks `/sys/class/power_supply/BAT*` once (cached) for an entry
//! exposing either `power_now` (microwatts) or
//! `current_now`+`voltage_now`. Reads on each sample. AC-only or no-
//! battery systems skip the column (Sample::power_w stays None).
//!
//! ## Why this lives next to the renderer
//!
//! When the user reports "scrolling heats up the CPU" we want the
//! number visible in the same window they're scrolling. Switching to
//! `htop` in another pane is too far removed; the moment-of-truth is
//! the keystroke that drives the spike.

use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    /// Process CPU% over the most recent sample interval. 0.0 if
    /// no prior sample exists yet, or if /proc reads failed.
    pub cpu_pct: f32,
    /// System battery discharge in watts, or None if not on battery
    /// or no readable power source. Proxy for total system power
    /// draw — when CPU spikes, this rises; lets the user spot the
    /// thermal cost of a feature in real time without an external
    /// tool.
    pub power_w: Option<f32>,
}

pub struct SysInfo {
    pid: u32,
    /// Path(s) for reading battery instantaneous power. Either a
    /// single `power_now` (microwatts), or the pair (current_now,
    /// voltage_now) (microamps × microvolts → multiply for power).
    power_source: Option<PowerSource>,
    /// utime+stime (clock ticks) at the last sample.
    last_proc_ticks: u64,
    /// Wall instant of the last sample.
    last_sample_at: Option<Instant>,
    /// Current cached sample shown in the HUD.
    pub cur: Sample,
    /// Throttle: don't re-sample more often than this.
    min_interval: std::time::Duration,
    /// Disabled by env var TERMPDF_NO_PERF_HUD=1.
    pub disabled: bool,
}

/// Two ways batteries expose instantaneous power on Linux:
///   - `power_now` in microwatts (most ACPI-style batteries).
///   - `current_now` (µA) × `voltage_now` (µV) (charge-controller
///     style, e.g. some Lenovo / older laptops). Both readings are
///     present on most kernels but `power_now` may be 0 while only
///     current+voltage are populated.
enum PowerSource {
    PowerNow(PathBuf),
    CurrentVoltage { current: PathBuf, voltage: PathBuf },
}

impl SysInfo {
    pub fn new() -> Self {
        let disabled = std::env::var("TERMPDF_NO_PERF_HUD")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false);
        let pid = std::process::id();
        let power_source = if disabled { None } else { find_power_source() };
        Self {
            pid,
            power_source,
            last_proc_ticks: 0,
            last_sample_at: None,
            cur: Sample::default(),
            min_interval: std::time::Duration::from_millis(1000),
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
        if let Some(last) = self.last_sample_at {
            if now.duration_since(last) < self.min_interval {
                return;
            }
        }
        let proc_ticks = read_proc_cpu_ticks(self.pid).unwrap_or(0);
        let power_w = self.power_source.as_ref().and_then(read_power_w);

        let cpu_pct = if let Some(last_at) = self.last_sample_at {
            let elapsed_ms = now.duration_since(last_at).as_millis().max(1) as u64;
            // proc ticks are in CLK_TCK units (100 Hz on every Linux
            // I've used). Convert to ms then divide by elapsed wall ms.
            let delta_ticks = proc_ticks.saturating_sub(self.last_proc_ticks);
            let delta_cpu_ms = delta_ticks * 1000 / clock_ticks_per_sec();
            (delta_cpu_ms as f32 / elapsed_ms as f32) * 100.0
        } else {
            0.0
        };

        self.cur = Sample { cpu_pct, power_w };
        self.last_proc_ticks = proc_ticks;
        self.last_sample_at = Some(now);
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

/// Find a CPU temperature sensor under /sys/class/hwmon. Returns the
/// `temp1_input` path of the first compatible device, or None. Cached
/// at startup so the per-sample read path is just one file read.
/// Find a battery's power-now reading. Walks /sys/class/power_supply
/// for entries whose `type` is "Battery" with a non-empty `power_now`
/// (microwatts) or fall-back `current_now` × `voltage_now` pair.
fn find_power_source() -> Option<PowerSource> {
    let root = std::path::Path::new("/sys/class/power_supply");
    let read = std::fs::read_dir(root).ok()?;
    for entry in read.flatten() {
        let dir = entry.path();
        let type_path = dir.join("type");
        let kind = std::fs::read_to_string(&type_path).ok();
        let is_battery = matches!(kind.as_deref().map(str::trim), Some("Battery"));
        if !is_battery {
            continue;
        }
        let power_now = dir.join("power_now");
        if power_now.exists() {
            // Quick sanity: it parses to a u64. Skip if file exists
            // but is empty / unreadable.
            if read_u64_file(&power_now).is_some() {
                return Some(PowerSource::PowerNow(power_now));
            }
        }
        let current = dir.join("current_now");
        let voltage = dir.join("voltage_now");
        if current.exists() && voltage.exists()
            && read_u64_file(&current).is_some()
            && read_u64_file(&voltage).is_some()
        {
            return Some(PowerSource::CurrentVoltage { current, voltage });
        }
    }
    None
}

fn read_power_w(src: &PowerSource) -> Option<f32> {
    match src {
        PowerSource::PowerNow(p) => {
            let uw = read_u64_file(p)?;
            // Microwatts → watts.
            Some((uw as f32) / 1_000_000.0)
        }
        PowerSource::CurrentVoltage { current, voltage } => {
            let ua = read_u64_file(current)?;
            let uv = read_u64_file(voltage)?;
            // µA × µV = µW × 10⁶, divide by 10¹² for W.
            // Use f64 in the middle to avoid u64 overflow on big batteries.
            let w = ((ua as f64) * (uv as f64)) / 1e12;
            Some(w as f32)
        }
    }
}

fn read_u64_file(p: &std::path::Path) -> Option<u64> {
    let s = std::fs::read_to_string(p).ok()?;
    s.trim().parse().ok()
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
        assert!(s.power_w.is_none());
    }

    #[test]
    fn sysinfo_disabled_returns_zeros() {
        // Direct construction without depending on env-state.
        let mut info = SysInfo {
            pid: 0,
            power_source: None,
            last_proc_ticks: 0,
            last_sample_at: None,
            cur: Sample::default(),
            min_interval: std::time::Duration::from_millis(0),
            disabled: true,
        };
        info.maybe_sample();
        assert_eq!(info.cur.cpu_pct, 0.0);
        assert!(info.cur.power_w.is_none());
    }

    #[test]
    fn first_sample_cpu_is_zero() {
        // No prior sample → no delta → 0%. The throttle is bypassed
        // here (min_interval = 0) so the call actually runs.
        let mut info = SysInfo {
            pid: std::process::id(),
            power_source: None,
            last_proc_ticks: 0,
            last_sample_at: None,
            cur: Sample::default(),
            min_interval: std::time::Duration::from_millis(0),
            disabled: false,
        };
        info.maybe_sample();
        assert_eq!(info.cur.cpu_pct, 0.0);
    }

    #[test]
    fn throttle_skips_repeat_calls() {
        // Two calls back-to-back within the throttle window — the
        // second must not reset last_sample_at to "now" (otherwise
        // the throttle wouldn't actually throttle).
        let mut info = SysInfo {
            pid: std::process::id(),
            power_source: None,
            last_proc_ticks: 0,
            last_sample_at: None,
            cur: Sample::default(),
            min_interval: std::time::Duration::from_secs(60),
            disabled: false,
        };
        info.maybe_sample();
        let first = info.last_sample_at;
        info.maybe_sample();
        let second = info.last_sample_at;
        assert_eq!(
            first, second,
            "throttled call should not update last_sample_at"
        );
    }
}
