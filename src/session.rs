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

/// Same atomic 0600 write as highlight.rs::write_private; duplicated
/// instead of pub-exposed to keep highlight.rs internals private and
/// because session.rs predates the security hardening.
fn write_private_session(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
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
