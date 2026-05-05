//! Sanitize strings for stderr / status-bar output.
//!
//! A malicious PDF can carry attacker-controlled text in an outline
//! title, embedded font name, or filename embedded by the user — and
//! pdfium-render's error variants tend to reflect those bytes back
//! out via `Display`. If we forward the raw `Display` to the user's
//! terminal, ANSI escape sequences in the input run as-is and can
//! repaint the prompt, fake a status line, or trigger DCS-based
//! exploits in older terminals.
//!
//! This is the LOW-severity finding from the security audit. The
//! mitigation is one line at every print site: strip C0/C1 control
//! characters except `\n` and `\t`, and replace them with `?`.
//!
//! Keep it tiny and dependency-free.

/// Strip ANSI / control bytes that a terminal would interpret.
///
/// Allowed through: printable Unicode (anything `>=0x20`), tab,
/// newline. Everything else (ESC, BEL, BS, DEL, C1 0x80–0x9F) is
/// replaced with `?`. The function works on `char`s, so it is safe
/// for arbitrary UTF-8 input.
pub fn safe_for_stderr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let code = ch as u32;
        let is_allowed = ch == '\n' || ch == '\t' || (0x20..0x7F).contains(&code) || code >= 0xA0;
        if is_allowed {
            out.push(ch);
        } else {
            out.push('?');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_plain_ascii() {
        assert_eq!(safe_for_stderr("hello world"), "hello world");
    }

    #[test]
    fn keeps_newline_and_tab() {
        assert_eq!(safe_for_stderr("a\nb\tc"), "a\nb\tc");
    }

    #[test]
    fn strips_csi_color_escape() {
        // \x1b[31m red, \x1b[0m reset
        let injected = "\x1b[31mFAKE\x1b[0m error: real-text";
        let cleaned = safe_for_stderr(injected);
        assert!(!cleaned.contains('\x1b'));
        assert!(cleaned.contains("FAKE"));
        assert!(cleaned.contains("real-text"));
    }

    #[test]
    fn strips_osc_dcs_bel() {
        let injected = "\x1b]0;evil\x07\x1bP1;1|leak\x1b\\";
        let cleaned = safe_for_stderr(injected);
        for c in cleaned.chars() {
            let code = c as u32;
            assert!(c == '\n' || c == '\t' || (0x20..0x7F).contains(&code) || code >= 0xA0);
        }
    }

    #[test]
    fn strips_c1_controls() {
        // C1 range 0x80-0x9F includes single-byte CSI/OSC equivalents
        // some terminals still honour.
        let injected = "before\u{0085}after\u{009B}5;5H";
        let cleaned = safe_for_stderr(injected);
        assert!(!cleaned.contains('\u{0085}'));
        assert!(!cleaned.contains('\u{009B}'));
    }

    #[test]
    fn keeps_unicode_text() {
        // Non-Latin text and emoji must pass through unchanged —
        // PDFs in non-English languages depend on this.
        let s = "café — 日本語 — 🌒";
        assert_eq!(safe_for_stderr(s), s);
    }

    #[test]
    fn strips_del() {
        assert_eq!(safe_for_stderr("a\x7Fb"), "a?b");
    }
}
