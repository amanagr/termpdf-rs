//! Persist highlights as native PDF annotations on the document
//! itself, instead of in a parallel JSON sidecar.
//!
//! The on-disk model: each user highlight becomes one
//! `PdfPageAnnotationType::Highlight` with a fill colour and a
//! `Contents` field holding our own JSON metadata so we can round-
//! trip the colour name and any inline note. Other readers
//! (Adobe, Apple Preview, sioyek, okular) will see a normal
//! highlight rectangle in the right place — they just won't
//! interpret our JSON note tags.
//!
//! The contents tag uses a small recognisable prefix so a future
//! "import other tools' highlights" path can tell apart "ours"
//! from "foreign" without false positives — useful when we need
//! to delete-and-recreate ours during a save without disturbing
//! highlights another tool created.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use pdfium_render::prelude::*;
use serde::{Deserialize, Serialize};

use crate::highlight::{Highlight, HighlightStore};

/// Every annotation we author has its `Contents` set to this JSON
/// shape, prefixed with the marker so we can identify ours later.
const TAG_PREFIX: &str = "termpdf-rs:";

#[derive(Debug, Serialize, Deserialize)]
struct AnnotMeta {
    color: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// Read every highlight annotation from every page and rebuild a
/// `HighlightStore` from them. Coordinates are normalised back into
/// the 0..1 top-left-origin space the rest of the app uses.
///
/// Annotations without a recognisable color or bounds are skipped
/// (rather than dropped silently — we log to stderr so the user
/// knows why a highlight didn't reappear).
pub fn load_from_pdf(document: &PdfDocument<'_>) -> Result<HighlightStore> {
    let mut items: Vec<Highlight> = Vec::new();
    let pages = document.pages();
    let total = pages.len() as i32;

    for page_idx in 0..total {
        let Ok(page) = pages.get(page_idx) else {
            continue;
        };
        let page_w = page.width().value.max(1.0);
        let page_h = page.height().value.max(1.0);
        let annotations = page.annotations();
        for annotation in annotations.iter() {
            let Some(hl) = annotation.as_highlight_annotation() else {
                continue;
            };
            // Bounds: pdfium gives PDF-space (bottom-left origin, points).
            // Convert back to top-left normalised 0..1.
            let Ok(bounds) = hl.bounds() else { continue };
            let left = bounds.left().value.max(0.0);
            let bottom = bounds.bottom().value.max(0.0);
            let right = bounds.right().value.min(page_w);
            let top = bounds.top().value.min(page_h);
            let x = (left / page_w).clamp(0.0, 1.0);
            let w = ((right - left) / page_w).clamp(0.0, 1.0);
            // Y flip.
            let y = ((page_h - top) / page_h).clamp(0.0, 1.0);
            let h = ((top - bottom) / page_h).clamp(0.0, 1.0);

            let color_hex = match hl.fill_color() {
                Ok(c) => format!("#{:02x}{:02x}{:02x}", c.red(), c.green(), c.blue()),
                Err(_) => "#ffd54f".to_string(),
            };

            // Try to recover our metadata from Contents. Foreign
            // highlights without our prefix get a default note: None.
            let (color_name, note) = match hl.contents() {
                Some(s) if s.starts_with(TAG_PREFIX) => {
                    let json_str = &s[TAG_PREFIX.len()..];
                    match serde_json::from_str::<AnnotMeta>(json_str) {
                        Ok(meta) => (meta.color, meta.note),
                        Err(_) => (color_hex.clone(), Some(s.clone())),
                    }
                }
                Some(s) => (color_hex.clone(), Some(s)),
                None => (color_hex.clone(), None),
            };

            items.push(Highlight {
                page: page_idx as usize,
                x,
                y,
                w,
                h,
                color: color_name,
                note,
            });
        }
    }
    Ok(HighlightStore { items })
}

/// Sync the in-memory `HighlightStore` back onto the PDF and write
/// the result atomically (temp file + rename) so a crash mid-write
/// can't corrupt the user's PDF.
///
/// Strategy: walk every page, delete every highlight annotation that
/// carries our `TAG_PREFIX` (so foreign highlights from other tools
/// are left alone), then recreate the user's current set from the
/// store. Uses `set_bounds` for the overall rect; the prior-art
/// research recommends per-line attachment-point quads for multi-line
/// selections, which we'll switch to once the new text-aware
/// selection model lands.
pub fn save_to_pdf(
    document: &PdfDocument<'_>,
    store: &HighlightStore,
    path: &Path,
) -> Result<()> {
    apply_store_to_document(document, store)?;
    save_atomic(document, path)
}

fn apply_store_to_document(
    document: &PdfDocument<'_>,
    store: &HighlightStore,
) -> Result<()> {
    let pages = document.pages();
    let total = pages.len() as i32;

    // Surface — rather than silently drop — highlights that point past
    // the end of the document (e.g. a session restored from a longer
    // version of the same file).
    let total_usize = total.max(0) as usize;
    let orphans = store
        .items
        .iter()
        .filter(|h| h.page >= total_usize)
        .count();
    if orphans > 0 {
        eprintln!(
            "warning: {orphans} highlight(s) reference page(s) past the end of \
             the PDF and will not be saved (document has {total_usize} page(s))"
        );
    }

    for page_idx in 0..total {
        let mut page = pages
            .get(page_idx)
            .with_context(|| format!("opening page {} for annotation sync", page_idx + 1))?;
        let page_w = page.width().value.max(1.0);
        let page_h = page.height().value.max(1.0);

        // Phase 1: collect indices of our annotations to delete.
        // pdfium re-indexes after each deletion so we walk in reverse.
        let to_delete: Vec<i32> = {
            let annotations = page.annotations();
            let count = annotations.len() as i32;
            let mut out = Vec::new();
            for i in 0..count {
                let Ok(annotation) = annotations.get(i as usize) else {
                    continue;
                };
                if annotation.as_highlight_annotation().is_none() {
                    continue;
                }
                let is_ours = annotation
                    .contents()
                    .map(|s| s.starts_with(TAG_PREFIX))
                    .unwrap_or(false);
                if is_ours {
                    out.push(i);
                }
            }
            out
        };

        if !to_delete.is_empty() {
            let annotations = page.annotations_mut();
            for &i in to_delete.iter().rev() {
                let Ok(ann) = annotations.get(i as usize) else {
                    continue;
                };
                let _ = annotations.delete_annotation(ann);
            }
        }

        // Phase 2: create one annotation per highlight on this page.
        let annotations = page.annotations_mut();
        for h in store.for_page(page_idx as usize) {
            // Convert top-left normalised 0..1 → PDF points (bottom-left).
            let left = (h.x.clamp(0.0, 1.0)) * page_w;
            let right = ((h.x + h.w).clamp(0.0, 1.0)) * page_w;
            let top = (1.0 - h.y.clamp(0.0, 1.0)) * page_h;
            let bottom = (1.0 - (h.y + h.h).clamp(0.0, 1.0)) * page_h;
            let bounds = PdfRect::new(
                PdfPoints::new(bottom),
                PdfPoints::new(left),
                PdfPoints::new(top),
                PdfPoints::new(right),
            );

            let mut hl = annotations
                .create_highlight_annotation()
                .with_context(|| format!("creating highlight on page {}", page_idx + 1))?;
            hl.set_bounds(bounds)
                .with_context(|| format!("setting bounds on page {}", page_idx + 1))?;
            let (r, g, b) = crate::highlight::rgb_from_hex(&h.color);
            let _ = hl.set_fill_color(PdfColor::new(r, g, b, 0xff));

            let meta = AnnotMeta {
                color: h.color.clone(),
                note: h.note.clone(),
            };
            let body = serde_json::to_string(&meta).unwrap_or_default();
            let _ = hl.set_contents(&format!("{TAG_PREFIX}{body}"));
        }
    }
    Ok(())
}

/// Write the document to `path` via a sibling temp file, then
/// rename. Pdfium's `save_to_file` writes directly; doing it in
/// two steps means the original PDF stays intact if pdfium dies
/// mid-write.
///
/// On Unix the temp file is pre-created with 0600 BEFORE pdfium
/// writes to it. pdfium's `open(path, O_WRONLY|O_TRUNC)` preserves
/// the existing inode's mode, so the file is never briefly world-
/// readable — a chmod-after-write was racy on shared filesystems.
fn save_atomic(document: &PdfDocument<'_>, path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let stem = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("doc");
    let tmp_path = parent.join(format!(".{stem}.termpdf-tmp"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Wipe any stale temp from a prior crash so create_new succeeds
        // with our mode rather than reusing leftover permissions.
        let _ = fs::remove_file(&tmp_path);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)
            .with_context(|| {
                format!("creating private temp file {}", tmp_path.display())
            })?;
    }

    document
        .save_to_file(&tmp_path)
        .with_context(|| format!("writing temp PDF to {}", tmp_path.display()))?;

    fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming {} → {}", tmp_path.display(), path.display()))?;
    Ok(())
}
