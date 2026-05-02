//! Persistent text highlights, keyed by canonical PDF path.
//!
//! Stored at `$XDG_DATA_HOME/termpdf-rs/<basename>.<hash>.json` so two
//! files with the same name in different directories don't collide.
//! Highlight coordinates are normalized 0..1 in PDF page space, so a
//! highlight stays in the right place even if the user reads at a
//! different zoom level later.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Cap on the basename portion of the on-disk filename. The full
/// stored name is `{stem}.{16-hex hash}.json`, so leaving headroom
/// keeps us comfortably under typical ext4/btrfs 255-byte limits.
const MAX_STEM_LEN: usize = 128;

/// Hard cap on the highlight-store JSON file size we'll attempt to
/// parse. A normal store with thousands of highlights is well under
/// 1 MiB; a hand-edited or hostile file in the GiB range would OOM
/// the process before serde even starts. 16 MiB is generous headroom.
const MAX_STORE_BYTES: u64 = 16 * 1024 * 1024;

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
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HighlightStore {
    pub items: Vec<Highlight>,
}

impl HighlightStore {
    pub fn store_path(pdf: &Path) -> Result<PathBuf> {
        let dir = dirs::data_local_dir()
            .ok_or_else(|| anyhow::anyhow!("$XDG_DATA_HOME not set and no fallback"))?
            .join("termpdf-rs");
        fs::create_dir_all(&dir)?;
        let raw_stem = pdf
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        let stem = sanitize_stem(raw_stem);
        let canon = fs::canonicalize(pdf).unwrap_or_else(|_| pdf.to_path_buf());
        let hash = fnv1a64(canon.to_string_lossy().as_bytes());
        Ok(dir.join(format!("{}.{:016x}.json", stem, hash)))
    }

    pub fn load(pdf: &Path) -> Result<Self> {
        let p = Self::store_path(pdf)?;
        if !p.exists() {
            return Ok(Self::default());
        }
        let len = fs::metadata(&p)
            .map(|m| m.len())
            .unwrap_or(0);
        if len > MAX_STORE_BYTES {
            anyhow::bail!(
                "highlight store {} is {} bytes (cap {}); refusing to load",
                p.display(),
                len,
                MAX_STORE_BYTES
            );
        }
        let data = fs::read_to_string(&p)?;
        // Propagate parse errors instead of silently substituting an
        // empty store. A subsequent clean exit would persist the
        // empty store back to disk, *destroying* the user's
        // highlights — better to bail and let the caller log the
        // path so they can recover the file by hand.
        serde_json::from_str(&data)
            .map_err(|e| anyhow::anyhow!("parsing {}: {}", p.display(), e))
    }

    /// Legacy sidecar-JSON writer. Kept for one release as a
    /// fallback for users downgrading from the PDF-annotation path.
    /// Live saves go through `pdfhighlights::save_to_pdf` instead.
    #[allow(dead_code)]
    pub fn save(&self, pdf: &Path) -> Result<()> {
        let p = Self::store_path(pdf)?;
        write_private(&p, serde_json::to_string_pretty(self)?.as_bytes())
    }

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

/// Atomic-ish write with restrictive 0600 permissions on Unix.
/// Highlights/notes can include sensitive excerpts of the PDF, so a
/// world-readable file in $XDG_DATA_HOME is a leak waiting to happen
/// on shared boxes. Writes via tempfile + rename so a crash mid-
/// write can't truncate the existing store.
#[allow(dead_code)]
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("highlights")
    ));
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            std::io::Write::write_all(&mut f, data)?;
            f.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            fs::write(&tmp, data)?;
        }
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
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

pub fn rgb_from_hex(hex: &str) -> (u8, u8, u8) {
    // Tolerate "#rrggbb" and bare "rrggbb"; fall back to yellow on
    // anything else so a hand-edited highlights.json doesn't crash.
    let h = hex.trim_start_matches('#');
    if h.len() == 6 {
        let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0xff);
        let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0xd5);
        let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(0x4f);
        return (r, g, b);
    }
    (0xff, 0xd5, 0x4f)
}
