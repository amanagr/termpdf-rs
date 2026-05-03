//! In-memory text-highlight store.
//!
//! Highlights live as native PDF annotations on the document itself
//! (see `pdfhighlights.rs`) — there is no parallel JSON sidecar.
//! Coordinates are normalised 0..1 in PDF page space so they stay
//! correct across zoom changes and re-renders.

use serde::{Deserialize, Serialize};

/// Cap on the basename portion of derived on-disk filenames (the
/// session file uses the same scheme). Keeps us comfortably under
/// typical ext4/btrfs 255-byte limits.
const MAX_STEM_LEN: usize = 128;

#[derive(Debug, Clone, Copy)]
pub struct Rect01 {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Highlight {
    pub page: usize,
    /// PDF coordinates, normalized to [0,1] (origin top-left).
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Hex color, e.g. "#ffd54f".
    pub color: String,
    /// Optional inline note typed at highlight time. Skipped when
    /// `None` so the on-disk JSON stays tidy and we don't ship a
    /// noisy `"note": null` for every highlight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// All Highlight entries created in a single user action share
    /// the same `group_id`. A multi-line selection saved with `y`
    /// becomes N rects on disk but ONE group; `x` deletes the whole
    /// group instead of leaving the user mashing `x` to clear what
    /// they made in one go. None on legacy / migrated highlights —
    /// those still delete one-rect-at-a-time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HighlightStore {
    pub items: Vec<Highlight>,
}

impl HighlightStore {
    pub fn for_page(&self, page: usize) -> impl Iterator<Item = &Highlight> {
        self.items.iter().filter(move |h| h.page == page)
    }

    pub fn add(&mut self, h: Highlight) {
        self.items.push(h);
    }
}

/// Re-export so session.rs (which mirrors the same hash-derived
/// store_path scheme) shares one sanitization implementation.
pub(crate) fn sanitize_stem_pub(raw: &str) -> String {
    sanitize_stem(raw)
}

/// Make a basename safe to embed in a derived filename: keep only
/// chars known to be benign on every supported FS, drop everything
/// else (including path separators, NUL, leading dots that would
/// hide the file). Empty result falls back to a generic label.
/// Length-cap protects against pathologically long names that would
/// blow filesystem `NAME_MAX` (255 on ext4/btrfs).
fn sanitize_stem(raw: &str) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Strip leading dots so the file isn't hidden, and leading
    // whitespace which some shells/tools mishandle.
    while s.starts_with(['.', ' ']) {
        s.remove(0);
    }
    if s.is_empty() {
        return "unknown".into();
    }
    if s.len() > MAX_STEM_LEN {
        s.truncate(MAX_STEM_LEN);
    }
    s
}

#[derive(Debug, Clone, Copy)]
pub struct HighlightColor {
    pub name: &'static str,
    pub hex: &'static str,
    /// (R, G, B) for fast in-memory blending — mirrors `hex` exactly.
    pub rgb: (u8, u8, u8),
}

/// Cyclable highlighter palette. Order is the cycle order on `c`.
pub const HIGHLIGHT_COLORS: &[HighlightColor] = &[
    HighlightColor { name: "yellow", hex: "#ffd54f", rgb: (0xff, 0xd5, 0x4f) },
    HighlightColor { name: "green",  hex: "#aed581", rgb: (0xae, 0xd5, 0x81) },
    HighlightColor { name: "blue",   hex: "#81d4fa", rgb: (0x81, 0xd4, 0xfa) },
    HighlightColor { name: "pink",   hex: "#f48fb1", rgb: (0xf4, 0x8f, 0xb1) },
    HighlightColor { name: "orange", hex: "#ffab91", rgb: (0xff, 0xab, 0x91) },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_path_separators() {
        assert_eq!(sanitize_stem("../../etc/passwd.pdf"), "_.._etc_passwd.pdf");
    }

    #[test]
    fn sanitize_strips_leading_dots() {
        // Without this, an attacker-controlled PDF named ".bashrc"
        // would write a hidden dotfile next to the user's real ones.
        assert_eq!(sanitize_stem(".hidden.pdf"), "hidden.pdf");
    }

    #[test]
    fn sanitize_drops_nul() {
        assert_eq!(sanitize_stem("ok\0evil.pdf"), "ok_evil.pdf");
    }

    #[test]
    fn sanitize_caps_length() {
        let big = "x".repeat(500);
        assert!(sanitize_stem(&big).len() <= MAX_STEM_LEN);
    }

    #[test]
    fn sanitize_empty_falls_back() {
        assert_eq!(sanitize_stem(""), "unknown");
        assert_eq!(sanitize_stem("..."), "unknown");
    }

    #[test]
    fn sanitize_keeps_normal_names() {
        assert_eq!(sanitize_stem("paper-2023_v2.pdf"), "paper-2023_v2.pdf");
    }
}

/// Parse `#rrggbb` or bare `rrggbb` into RGB. Any malformed input
/// falls back to yellow as a *whole* — a partial parse used to
/// produce Frankenstein colors (e.g. `#zz0000` → red instead of
/// yellow), which silently disagreed with the saved hex string.
///
/// The ASCII guard catches corner cases like a 6-byte string that
/// contains a multi-byte UTF-8 char (e.g. emoji-derived junk on a
/// hostile PDF Contents field). Without it, slicing on a non-char
/// boundary would panic the renderer.
pub fn rgb_from_hex(hex: &str) -> (u8, u8, u8) {
    const FALLBACK: (u8, u8, u8) = (0xff, 0xd5, 0x4f);
    let h = hex.trim_start_matches('#');
    if h.len() != 6 || !h.is_ascii() {
        return FALLBACK;
    }
    match (
        u8::from_str_radix(&h[0..2], 16),
        u8::from_str_radix(&h[2..4], 16),
        u8::from_str_radix(&h[4..6], 16),
    ) {
        (Ok(r), Ok(g), Ok(b)) => (r, g, b),
        _ => FALLBACK,
    }
}

#[cfg(test)]
mod color_tests {
    use super::rgb_from_hex;

    #[test]
    fn valid_hex_parses() {
        assert_eq!(rgb_from_hex("#aabbcc"), (0xaa, 0xbb, 0xcc));
        assert_eq!(rgb_from_hex("aabbcc"), (0xaa, 0xbb, 0xcc));
    }

    #[test]
    fn malformed_hex_falls_back_consistently() {
        // Was a real bug: partial parse used different per-channel
        // defaults, so "#zz0000" returned (ff, 00, 00) — red, not the
        // documented yellow fallback. Now any parse failure on any
        // channel returns yellow as a whole.
        assert_eq!(rgb_from_hex("#zz0000"), (0xff, 0xd5, 0x4f));
        assert_eq!(rgb_from_hex("#zzzzzz"), (0xff, 0xd5, 0x4f));
        assert_eq!(rgb_from_hex(""), (0xff, 0xd5, 0x4f));
        assert_eq!(rgb_from_hex("#abc"), (0xff, 0xd5, 0x4f));
    }

    /// Hostile / corrupt PDF could embed a 6-byte color string that
    /// contains a multi-byte UTF-8 char (e.g. "🦀ab" — 6 bytes, 3
    /// chars). The previous len-only guard let those through; slicing
    /// on a non-char boundary would panic the renderer. Defence-in-
    /// depth: bail to fallback when the string isn't pure ASCII.
    #[test]
    fn non_ascii_six_byte_input_falls_back_without_panic() {
        // 🦀 is 4 bytes + "ab" = 6 bytes total but contains a
        // multi-byte char.
        assert_eq!(rgb_from_hex("🦀ab"), (0xff, 0xd5, 0x4f));
        // Mid-position multi-byte: "aï0000" (a=1, ï=2, 0=1, 0=1, 0=1, 0=1 ... wait this is 7 bytes).
        // Use "ïïï" (3 chars × 2 bytes each = 6 bytes).
        assert_eq!(rgb_from_hex("ïïï"), (0xff, 0xd5, 0x4f));
    }
}
