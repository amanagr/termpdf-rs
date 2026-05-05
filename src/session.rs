//! Per-document session state — last page + dark-mode flag.
//!
//! Stored at `$XDG_DATA_HOME/termpdf-rs/<basename>.<hash>.session.json`,
//! parallel to the highlight store, so reopening a PDF lands the user
//! on the page they left and respects their last dark/light choice.
//! Same file-keying scheme as `highlight.rs`: basename + fnv1a64 of the
//! canonical path, so two PDFs with the same name in different
//! directories don't collide.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// 0-indexed page number the user was on at exit.
    #[serde(default)]
    pub page: usize,
    /// Dark mode flag at exit.
    #[serde(default)]
    pub dark: bool,
    /// Zoom factor at exit (1.0 = fit-width).
    #[serde(default = "default_zoom")]
    pub zoom: f32,
    /// Vim-style named marks `m{a..z}` → page index. Persisted so
    /// `'a` after a reopen lands where the user expected.
    #[serde(default)]
    pub marks: std::collections::BTreeMap<char, usize>,
    /// Fraction `0.0..=1.0` of the way down the saved page at exit.
    /// Stored as fraction (not absolute pixels) so the restored
    /// position survives terminal resize / zoom drift between sessions.
    /// 0.0 = at top of page; serde default keeps legacy sessions
    /// landing at page top, matching pre-feature behaviour.
    #[serde(default)]
    pub scroll_in_page: f32,
    /// Last-used highlight color index (0..HIGHLIGHT_COLORS.len()).
    /// Persisted so the user's preferred color survives a reopen
    /// instead of resetting to 0 every session.
    #[serde(default)]
    pub selection_color_idx: usize,
}

fn default_zoom() -> f32 {
    1.0
}

impl Default for Session {
    fn default() -> Self {
        Self {
            page: 0,
            dark: false,
            zoom: 1.0,
            marks: std::collections::BTreeMap::new(),
            scroll_in_page: 0.0,
            selection_color_idx: 0,
        }
    }
}

impl Session {
    pub fn store_path(pdf: &Path) -> Result<PathBuf> {
        let dir = dirs::data_local_dir()
            .ok_or_else(|| anyhow::anyhow!("$XDG_DATA_HOME not set and no fallback"))?
            .join("termpdf-rs");
        fs::create_dir_all(&dir)?;
        let raw_stem = pdf
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        let stem = crate::highlight::sanitize_stem_pub(raw_stem);
        let canon = fs::canonicalize(pdf).unwrap_or_else(|_| pdf.to_path_buf());
        let hash = fnv1a64(canon.to_string_lossy().as_bytes());
        Ok(dir.join(format!("{}.{:016x}.session.json", stem, hash)))
    }

    /// Read the saved session for this PDF. A missing or unparseable
    /// file is treated as "no saved state" rather than a hard error —
    /// a stale on-disk format from an older version shouldn't block
    /// the reader from opening.
    pub fn load(pdf: &Path) -> Self {
        let Ok(p) = Self::store_path(pdf) else {
            return Self::default();
        };
        if !p.exists() {
            return Self::default();
        }
        match fs::read_to_string(&p) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, pdf: &Path) -> Result<()> {
        let p = Self::store_path(pdf)?;
        let body = serde_json::to_string_pretty(self)?;
        write_private_session(&p, body.as_bytes())
    }
}

/// Same atomic 0600 write as `pdfhighlights::save_atomic`. Mitigates
/// two same-UID concerns from the deterministic-tempfile pattern that
/// used to live here:
///   1. **Symlink-write attack**: an attacker pre-plants
///      `.{filename}.tmp` as a symlink to a victim file (~/.bashrc,
///      etc.). Our open follows the link, truncates the target, and
///      writes session JSON there. `O_NOFOLLOW + O_EXCL` defeats
///      this — the open fails noisily on either symlink-or-existing.
///   2. **Concurrent-instance write race**: two termpdf-rs procs on
///      the same PDF both open the deterministic temp path with
///      truncate; their writes interleave and rename loses the
///      loser. A pid+nanos suffix guarantees a unique temp path
///      per write.
fn write_private_session(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(
        ".{}.{pid}.{nanos}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
    ));
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            const O_NOFOLLOW: i32 = 0x20000;
            const O_EXCL: i32 = 0o0200;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .custom_flags(O_NOFOLLOW | O_EXCL)
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
    // rename is atomic on the same filesystem; if it fails we leave
    // the (now-unique) temp behind to be reaped by future opens.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_pdf_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "termpdf-session-{}-{}.pdf",
            std::process::id(),
            tag
        ));
        // The path doesn't have to exist; Session uses canonicalize
        // best-effort and falls back to the raw path for hashing.
        let _ = std::fs::write(&p, b"%PDF-1.4\n");
        p
    }

    #[test]
    fn save_then_load_roundtrips_all_fields() {
        let pdf = temp_pdf_path("rt");
        let mut marks = std::collections::BTreeMap::new();
        marks.insert('a', 12);
        marks.insert('z', 99);
        let s = Session {
            page: 42,
            dark: true,
            zoom: 2.5,
            marks,
            scroll_in_page: 0.37,
            selection_color_idx: 3,
        };
        s.save(&pdf).unwrap();
        let loaded = Session::load(&pdf);
        assert_eq!(loaded.page, 42);
        assert!(loaded.dark);
        assert!((loaded.zoom - 2.5).abs() < 1e-6);
        assert_eq!(loaded.marks.get(&'a'), Some(&12));
        assert_eq!(loaded.marks.get(&'z'), Some(&99));
        assert!((loaded.scroll_in_page - 0.37).abs() < 1e-6);
        assert_eq!(loaded.selection_color_idx, 3);
        // Cleanup
        let _ = std::fs::remove_file(Session::store_path(&pdf).unwrap());
        let _ = std::fs::remove_file(&pdf);
    }

    #[test]
    fn missing_session_file_returns_default() {
        // Use a PDF path that will produce a unique-but-nonexistent
        // store path. Session::load should NOT bail; defaults instead.
        let pdf = std::env::temp_dir().join(format!(
            "termpdf-no-session-{}-{}.pdf",
            std::process::id(),
            "missing"
        ));
        let _ = std::fs::remove_file(Session::store_path(&pdf).unwrap_or_default());
        let s = Session::load(&pdf);
        assert_eq!(s.page, 0);
        assert!(!s.dark);
        assert!((s.zoom - 1.0).abs() < 1e-6);
        assert!(s.marks.is_empty());
        assert_eq!(s.scroll_in_page, 0.0);
    }

    #[test]
    fn corrupt_session_file_returns_default() {
        let pdf = temp_pdf_path("corrupt");
        let store = Session::store_path(&pdf).unwrap();
        std::fs::write(&store, b"not json {{{").unwrap();
        let s = Session::load(&pdf);
        // Hard-fail on a corrupt file would block the user from
        // ever opening this PDF again — defaulting is the right call.
        assert_eq!(s.page, 0);
        let _ = std::fs::remove_file(&store);
        let _ = std::fs::remove_file(&pdf);
    }

    #[test]
    fn legacy_session_without_zoom_or_marks_loads() {
        // Older sessions only had `page` and `dark`. Make sure the
        // serde defaults kick in cleanly.
        let pdf = temp_pdf_path("legacy");
        let store = Session::store_path(&pdf).unwrap();
        std::fs::write(&store, br#"{"page":7,"dark":true}"#).unwrap();
        let s = Session::load(&pdf);
        assert_eq!(s.page, 7);
        assert!(s.dark);
        assert!((s.zoom - 1.0).abs() < 1e-6);
        assert!(s.marks.is_empty());
        // scroll_in_page is the newest field; legacy sessions get 0.0
        // (top of page) so reopens from old saves don't surprise users.
        assert_eq!(s.scroll_in_page, 0.0);
        let _ = std::fs::remove_file(&store);
        let _ = std::fs::remove_file(&pdf);
    }
}
