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
    /// Optional inline note typed at highlight time.
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
        let stem = pdf
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        let canon = fs::canonicalize(pdf).unwrap_or_else(|_| pdf.to_path_buf());
        let hash = fnv1a64(canon.to_string_lossy().as_bytes());
        Ok(dir.join(format!("{}.{:016x}.json", stem, hash)))
    }

    pub fn load(pdf: &Path) -> Result<Self> {
        let p = Self::store_path(pdf)?;
        if !p.exists() {
            return Ok(Self::default());
        }
        let data = fs::read_to_string(&p)?;
        Ok(serde_json::from_str(&data).unwrap_or_default())
    }

    pub fn save(&self, pdf: &Path) -> Result<()> {
        let p = Self::store_path(pdf)?;
        fs::write(p, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Reserved for v0.2 — once the renderer overlays saved highlights,
    /// `ui::draw` will iterate per-page via this. Keep `dead_code`
    /// silenced rather than deleting it; the alternative is reintroducing
    /// the API in v0.2 with a different name.
    #[allow(dead_code)]
    pub fn for_page(&self, page: usize) -> impl Iterator<Item = &Highlight> {
        self.items.iter().filter(move |h| h.page == page)
    }

    /// Reserved for v0.2 — visual mode's `y` will call this once mouse
    /// drag → page-space coordinate plumbing lands.
    #[allow(dead_code)]
    pub fn add(&mut self, h: Highlight) {
        self.items.push(h);
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
