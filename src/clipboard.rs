//! Copy a chunk of UTF-8 text to the user's clipboard, doing the
//! best we can across local Wayland, X11, and remote SSH sessions.
//!
//! Strategy:
//!   1. Always emit OSC 52 to /dev/tty. This is the only mechanism
//!      that crosses SSH unmodified, and Kitty/WezTerm/foot/Ghostty
//!      all support it out of the box. Inside tmux, wrap with the
//!      passthrough escape sequence.
//!   2. *Also* attempt a native copy (wl-copy / xclip / xsel) when
//!      the user is on a local seat — duplicate writes to the same
//!      clipboard are harmless and the native binary populates
//!      paste targets that some GUI apps still prefer.
//!
//! Both writes are best-effort. The function never errors out for
//! "couldn't copy"; the worst case is the user pastes nothing.

use std::io::Write;
use std::process::{Command, Stdio};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

/// Soft cap on the bytes we'll attempt to copy. Kitty's OSC 52
/// ceiling is ~8 MB; tmux's is much lower depending on
/// `set-clipboard` config. Academic-paper selections are typically
/// well under 2 KB; anything past 75 KB pre-encode is almost
/// certainly a runaway and not what the user wanted to copy.
pub const MAX_COPY_BYTES: usize = 75 * 1024;

#[derive(Debug, Default)]
pub struct CopyOutcome {
    pub bytes: usize,
    pub osc52_written: bool,
    pub native_written: bool,
    pub truncated: bool,
}

pub fn copy(text: &str) -> CopyOutcome {
    // Strip terminal-execution chars BEFORE truncation. Selection
    // text comes from the PDF text layer and is attacker-controlled —
    // a hostile PDF can embed CSI/OSC bytes inside extractable
    // glyphs. Both sinks (OSC 52 over /dev/tty AND the native binary
    // via stdin) deliver bytes the user later pastes into a shell;
    // unsanitised escapes execute on paste. `safe_for_clipboard`
    // keeps bidi/zero-width chars (legitimate in international text)
    // but drops C0/C1 controls and line separators.
    let sanitised = crate::term_safe::safe_for_clipboard(text);
    let mut payload: &str = &sanitised;
    let truncated = payload.len() > MAX_COPY_BYTES;
    if truncated {
        // Truncate at a char boundary so we don't slice through a
        // multi-byte UTF-8 codepoint.
        payload = trim_to_char_boundary(payload, MAX_COPY_BYTES);
    }

    let mut out = CopyOutcome {
        bytes: payload.len(),
        truncated,
        ..Default::default()
    };

    // OSC 52 is the SSH-friendly path; always attempt it.
    if osc52_emit(payload).is_ok() {
        out.osc52_written = true;
    }

    // Native binary, but skip if we're remote — the user's local
    // clipboard is what matters then, and OSC 52 already handles it.
    if std::env::var("SSH_CONNECTION").is_err() && spawn_native(payload).is_ok() {
        out.native_written = true;
    }

    out
}

fn osc52_emit(text: &str) -> std::io::Result<()> {
    let encoded = B64.encode(text.as_bytes());
    // OSC 52 ; c ; <base64> BEL — BEL terminator is more
    // tmux-friendly than ST.
    let inner = format!("\x1b]52;c;{}\x07", encoded);

    // Inside tmux, wrap with the passthrough sequence. Doubles any
    // embedded ESC byte. Requires `set -g allow-passthrough on`.
    let payload = if std::env::var("TMUX").is_ok() {
        let escaped = inner.replace('\x1b', "\x1b\x1b");
        format!("\x1bPtmux;{}\x1b\\", escaped)
    } else {
        inner
    };

    // Write to /dev/tty so the escape sequence reaches the terminal
    // even while ratatui owns stdout.
    let mut tty = std::fs::OpenOptions::new().write(true).open("/dev/tty")?;
    tty.write_all(payload.as_bytes())?;
    tty.flush()?;
    Ok(())
}

/// Try wl-copy (Wayland), xclip (X11), then xsel (X11 fallback).
/// First binary that spawns AND exits successfully wins.
///
/// Critical: `xclip` without `-loops 1` blocks until the first paste
/// (it owns the X11 selection synchronously) — `child.wait()` would
/// hang the whole TUI. We pass `-loops 1` so xclip exits after one
/// paste-or-clear, and we always check `status.success()` before
/// declaring victory so a binary-on-PATH-but-misconfigured (e.g.
/// `wl-copy` with no Wayland socket) falls through to the next.
fn spawn_native(text: &str) -> std::io::Result<()> {
    let candidates: &[(&str, &[&str])] = if std::env::var("WAYLAND_DISPLAY").is_ok() {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard", "-loops", "1"]),
            ("xsel", &["-b", "-i"]),
        ]
    } else {
        &[
            ("xclip", &["-selection", "clipboard", "-loops", "1"]),
            ("xsel", &["-b", "-i"]),
            ("wl-copy", &[]),
        ]
    };

    for (cmd, args) in candidates {
        let Ok(mut child) = Command::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        // Drop stdin so the child sees EOF and can exit. Without
        // this, the child blocks on read and we deadlock here.
        let status = match child.wait() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if status.success() {
            return Ok(());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no working native clipboard binary on PATH",
    ))
}

fn trim_to_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut idx = max;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    &s[..idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_to_char_boundary_keeps_full_string_when_short() {
        assert_eq!(trim_to_char_boundary("hello", 100), "hello");
    }

    #[test]
    fn trim_to_char_boundary_walks_back_off_multibyte() {
        // "é" is 2 bytes; a request to slice at byte 1 must walk back
        // to byte 0, returning the empty string rather than panic.
        let s = "é";
        assert_eq!(trim_to_char_boundary(s, 1), "");
    }

    #[test]
    fn trim_to_char_boundary_handles_mixed_ascii_and_utf8() {
        let s = "ab\u{1F600}cd"; // 'ab' + 4-byte emoji + 'cd' = 8 bytes
                                 // Request 4 bytes: 'ab' fits at boundary 2; emoji starts at 2
                                 // and is 4 bytes long, so 4 is mid-emoji → walk back to 2.
        assert_eq!(trim_to_char_boundary(s, 4), "ab");
    }
}
