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
/// newline. Replaced with `?`:
///   - C0 controls (ESC, BEL, BS, DEL, …)
///   - C1 controls (0x80–0x9F)
///   - U+2028 LINE SEPARATOR / U+2029 PARAGRAPH SEPARATOR (treated
///     as line breaks by some terminals — can split a status line)
///   - Bidi-override class (U+200E/U+200F LTR/RTL marks,
///     U+202A–U+202E embedding/override, U+2066–U+2069 isolates).
///     A malicious PDF outline title with these can flip the visual
///     rendering of subsequent characters in the status line.
pub fn safe_for_stderr(s: &str) -> String {
    // Hot-path: pure ASCII text (status bar's common case — paths,
    // page numbers, error messages). Skip the per-char walk and
    // return owned-without-rebuilding when nothing needs rewriting.
    if s.bytes()
        .all(|b| b == b'\n' || b == b'\t' || (0x20..0x7F).contains(&b))
    {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let code = ch as u32;
        let is_bidi = matches!(
            code,
            0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069
        );
        let is_separator = code == 0x2028 || code == 0x2029;
        // Zero-width / format-effect characters that shift cursor
        // alignment without occupying a visible cell — terminals
        // disagree about handling, status-bar widths drift. ZWSP /
        // ZWNJ / ZWJ / WJ / BOM / variation selectors / Mongolian
        // selectors / tag characters (the U+E0000-block exploit
        // surface used in the 2021 iMessage trick).
        let is_zero_width = matches!(
            code,
            0x200B..=0x200D
                | 0x2060
                | 0xFEFF
                | 0xFE00..=0xFE0F
                | 0x180B..=0x180D
                | 0xE0000..=0xE007F
        );
        let is_allowed = !is_bidi
            && !is_separator
            && !is_zero_width
            && (ch == '\n' || ch == '\t' || (0x20..0x7F).contains(&code) || code >= 0xA0);
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

    #[test]
    fn strips_bidi_overrides() {
        // RTL override sandwich — left-side text renders mirrored if
        // the terminal honours U+202E.
        let injected = "safe\u{202E}txet_yldneirf\u{202C}rest";
        let cleaned = safe_for_stderr(injected);
        assert!(!cleaned.contains('\u{202E}'));
        assert!(!cleaned.contains('\u{202C}'));
    }

    #[test]
    fn strips_line_separators() {
        // U+2028 / U+2029 — some terminals treat these as line
        // breaks, splitting a status string across rows.
        let injected = "before\u{2028}after\u{2029}end";
        let cleaned = safe_for_stderr(injected);
        assert!(!cleaned.contains('\u{2028}'));
        assert!(!cleaned.contains('\u{2029}'));
    }

    #[test]
    fn strips_zero_width_and_bom() {
        // BOM / ZWSP / variation selector — width-drift in status bar.
        let injected = "a\u{FEFF}b\u{200B}c\u{FE0F}d";
        let cleaned = safe_for_stderr(injected);
        assert!(!cleaned.contains('\u{FEFF}'));
        assert!(!cleaned.contains('\u{200B}'));
        assert!(!cleaned.contains('\u{FE0F}'));
        assert!(cleaned.contains('a') && cleaned.contains('d'));
    }
}
