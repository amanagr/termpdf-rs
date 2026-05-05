//! Optional debug logging for the kitty graphics path.
//!
//! Activated by setting `TERMPDF_DEBUG_LOG=/path/to/log.txt` in the
//! environment. The first call from anywhere opens the file in append
//! mode (NO truncate — see `handle()` below for the data-loss guard);
//! subsequent calls append timestamped one-line records. When the env
//! var is unset, every call short-circuits to a no-op (one
//! `OnceLock::get()` and a None match) so production builds pay no
//! cost beyond that.
//!
//! The logger writes to a file rather than stderr because stderr in
//! a `termpdf` session is the user's terminal — anything we print
//! there corrupts the rendered screen. A file lets the user `tail
//! -f` from a separate pane while reproducing.
//!
//! Format is a single line per event:
//!
//! ```text
//!     <ms_since_start>  <category>  k1=v1 k2=v2 ...
//! ```
//!
//! Keep the field set tight per call site — wide records make the
//! eventual `grep` annoying.

use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static LOG: OnceLock<Option<Mutex<File>>> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

fn handle() -> Option<&'static Mutex<File>> {
    LOG.get_or_init(|| {
        let path = std::env::var("TERMPDF_DEBUG_LOG").ok()?;
        // Defenses against accidental data loss / symlink hijack:
        //   * O_NOFOLLOW — refuse to follow a symlink at the final path
        //     component. If `~/.termpdf-log` is a symlink to ~/.bashrc
        //     (an attacker- or self-induced typo), don't truncate the
        //     target. EOPNOTSUPP / ELOOP returns Err → opt out of logs.
        //   * No `truncate(true)` — append instead. Past behaviour
        //     silently overwrote any file the user pointed the env var
        //     at; appending preserves prior content if the user
        //     mistypes (e.g. `TERMPDF_DEBUG_LOG=~/.bashrc` for the
        //     duration of one session is recoverable, not a wipe).
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            const O_NOFOLLOW: i32 = 0x20000; // matches pdfhighlights.rs / disk_cache.rs
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .custom_flags(O_NOFOLLOW)
                .open(&path)
                .ok()?
        };
        #[cfg(not(unix))]
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        let _ = START.set(Instant::now());
        Some(Mutex::new(file))
    })
    .as_ref()
}

/// True when `TERMPDF_DEBUG_LOG` is set and the log file was opened
/// successfully. Use to gate any `format!`-heavy calls so disabled
/// builds don't pay the formatting cost.
pub fn enabled() -> bool {
    handle().is_some()
}

/// Emit a single line. `category` is the event family (e.g. "transmit",
/// "place", "evict"); `body` is the already-formatted "k=v k=v" tail.
/// Caller formats so each call site can pick its field set without
/// allocations from a generic `&[(&str, &str)]` API.
pub fn write(category: &str, body: &str) {
    let Some(h) = handle() else {
        return;
    };
    let elapsed = START.get().map(|s| s.elapsed().as_millis()).unwrap_or(0);
    if let Ok(mut f) = h.lock() {
        let _ = writeln!(f, "{elapsed:>8}  {category}  {body}");
        let _ = f.flush();
    }
}
