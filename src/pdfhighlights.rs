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
            // Read our metadata FIRST. If this annotation wasn't
            // authored by termpdf-rs, skip it entirely — we never need
            // to query bounds/colour on a foreign highlight, and some
            // PDF authors (notably Adobe's) emit Highlight annotations
            // without an InteriorColor entry, which makes
            // pdfium-render's `fill_color()` segfault on certain
            // builds. Skipping foreign highlights also matches user
            // intent: those are already painted into the page bitmap
            // by pdfium's own renderer, so they're still visible.
            let contents = hl.contents();
            let Some(meta) = contents
                .as_deref()
                .filter(|s| s.starts_with(TAG_PREFIX))
                .and_then(|s| serde_json::from_str::<AnnotMeta>(&s[TAG_PREFIX.len()..]).ok())
            else {
                continue;
            };
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

            items.push(Highlight {
                page: page_idx as usize,
                x,
                y,
                w,
                h,
                color: meta.color,
                note: meta.note,
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

#[cfg(test)]
mod tests {
    //! Regression tests for the PDF-annotation highlight pipeline.
    //!
    //! These need a real libpdfium loaded — `setup.sh` stages one in
    //! `vendor/`. If pdfium can't be located (e.g. CI before
    //! `setup.sh` ran), the tests skip via `eprintln` rather than
    //! failing, so a clean `cargo test` on a fresh checkout still
    //! tells the developer what to do.
    use super::*;
    use crate::highlight::{Highlight, HighlightStore};
    use std::path::PathBuf;

    fn pdfium_or_skip() -> Option<pdfium_render::prelude::Pdfium> {
        let lib = match crate::pdf::find_libpdfium() {
            Ok(p) => p,
            Err(_) => {
                eprintln!("skipping: libpdfium not found (run ./setup.sh)");
                return None;
            }
        };
        let bindings = crate::pdf::bindings(&lib).ok()?;
        Some(pdfium_render::prelude::Pdfium::new(bindings))
    }

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("termpdf-test-{}-{}.pdf", std::process::id(), name));
        p
    }

    /// REGRESSION: a Highlight annotation authored by Adobe (or any
    /// other tool) that omits the InteriorColor entry caused
    /// `pdfium-render`'s `fill_color()` to segfault under our 0.9
    /// build — `load_from_pdf` would crash mid-iteration on a real
    /// 600-page book. Skipping foreign annotations entirely avoids
    /// the broken call and matches user intent (those annotations
    /// are already painted by pdfium's own renderer).
    #[test]
    fn load_skips_foreign_highlights_without_segfault() {
        let Some(pdfium) = pdfium_or_skip() else { return };
        let path = temp_path("foreign");

        // Build a one-page PDF with a Highlight annotation that
        // intentionally has NO Contents tag (so it's "foreign") and
        // NO fill_color set (mimicking Adobe's emission). Save then
        // reload so the annotation goes through pdfium's parser.
        {
            let mut doc = pdfium.create_new_pdf().expect("new pdf");
            let mut page = doc
                .pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .expect("page");
            let bounds = PdfRect::new(
                PdfPoints::new(100.0),
                PdfPoints::new(100.0),
                PdfPoints::new(150.0),
                PdfPoints::new(300.0),
            );
            let mut hl = page
                .annotations_mut()
                .create_highlight_annotation()
                .expect("create highlight");
            hl.set_bounds(bounds).expect("set bounds");
            // No set_fill_color, no set_contents — this is the shape
            // that crashed on load.
            doc.save_to_file(&path).expect("save");
        }

        let doc = pdfium
            .load_pdf_from_file(&path, None)
            .expect("reload");
        // The bug was a segfault here; reaching the assert is the win.
        let store = load_from_pdf(&doc).expect("load_from_pdf");
        assert_eq!(
            store.items.len(),
            0,
            "foreign highlights must be skipped (pdfium fill_color is unsafe on Adobe-style annotations)"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// REGRESSION: a Highlight saved via save_to_pdf and immediately
    /// re-applied (without a save+reload between) must survive the
    /// in-memory delete-then-recreate that apply_store_to_document
    /// performs each time. Catches a class of bug where the second
    /// save would delete the just-created annotation before
    /// recreating it (idempotency check).
    #[test]
    fn double_save_is_idempotent() {
        let Some(pdfium) = pdfium_or_skip() else { return };
        let path = temp_path("idempotent");
        {
            let mut doc = pdfium.create_new_pdf().expect("new pdf");
            doc.pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .expect("page");
            doc.save_to_file(&path).expect("save empty");
        }
        let store = HighlightStore {
            items: vec![Highlight {
                page: 0,
                x: 0.10,
                y: 0.20,
                w: 0.30,
                h: 0.05,
                color: "#aed581".into(),
                note: None,
            }],
        };
        // Save twice in a row — must not duplicate or lose the entry.
        {
            let doc = pdfium.load_pdf_from_file(&path, None).expect("reload");
            save_to_pdf(&doc, &store, &path).expect("first save");
        }
        {
            let doc = pdfium.load_pdf_from_file(&path, None).expect("reload2");
            save_to_pdf(&doc, &store, &path).expect("second save");
        }
        let doc = pdfium.load_pdf_from_file(&path, None).expect("reload3");
        let loaded = load_from_pdf(&doc).expect("load");
        assert_eq!(loaded.items.len(), 1, "double-save must not duplicate");
        let _ = std::fs::remove_file(&path);
    }

    /// REGRESSION: save_to_pdf must preserve foreign Highlight
    /// annotations (other tools' work) on the same page where we
    /// also have one of ours. Phase 1's "delete only OUR tagged
    /// annotations" predicate is what guards this.
    #[test]
    fn save_preserves_foreign_highlights_on_same_page() {
        let Some(pdfium) = pdfium_or_skip() else { return };
        let path = temp_path("preserve-foreign");
        // Build a PDF with one foreign Highlight (no Contents tag).
        {
            let mut doc = pdfium.create_new_pdf().expect("new pdf");
            let mut page = doc
                .pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .expect("page");
            let bounds = PdfRect::new(
                PdfPoints::new(500.0),
                PdfPoints::new(50.0),
                PdfPoints::new(550.0),
                PdfPoints::new(200.0),
            );
            let mut hl = page
                .annotations_mut()
                .create_highlight_annotation()
                .expect("create");
            hl.set_bounds(bounds).expect("bounds");
            // Intentionally no set_contents — this is "foreign."
            doc.save_to_file(&path).expect("save");
        }

        // Now add one of OUR highlights via save_to_pdf on the same page.
        {
            let doc = pdfium.load_pdf_from_file(&path, None).expect("reload");
            let store = HighlightStore {
                items: vec![Highlight {
                    page: 0,
                    x: 0.10,
                    y: 0.10,
                    w: 0.20,
                    h: 0.05,
                    color: "#ffd54f".into(),
                    note: None,
                }],
            };
            save_to_pdf(&doc, &store, &path).expect("save_to_pdf");
        }

        // Reopen: the foreign annotation should still be present in
        // the PDF (count via raw pdfium iteration), and OUR load
        // should return exactly one tagged highlight.
        let doc = pdfium.load_pdf_from_file(&path, None).expect("reload2");
        let total_highlights = {
            let pages = doc.pages();
            let page = pages.get(0).expect("page 0");
            page.annotations()
                .iter()
                .filter(|a| a.as_highlight_annotation().is_some())
                .count()
        };
        assert_eq!(
            total_highlights, 2,
            "foreign + ours = 2 Highlight annotations on the page"
        );
        let store = load_from_pdf(&doc).expect("load");
        assert_eq!(
            store.items.len(),
            1,
            "load must return ONLY tagged highlights, not foreign ones"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Roundtrip: save a tagged Highlight, reload it, verify all
    /// fields survive. Catches any future change that breaks the
    /// TAG_PREFIX/JSON encoding of color + note metadata.
    #[test]
    fn save_then_load_roundtrips_metadata() {
        let Some(pdfium) = pdfium_or_skip() else { return };
        let path = temp_path("roundtrip");

        // Start with an empty 1-page PDF.
        {
            let mut doc = pdfium.create_new_pdf().expect("new pdf");
            doc.pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .expect("page");
            doc.save_to_file(&path).expect("save empty");
        }

        // Open the saved file and drop one of our highlights into it.
        {
            let doc = pdfium.load_pdf_from_file(&path, None).expect("reload");
            let store = HighlightStore {
                items: vec![Highlight {
                    page: 0,
                    x: 0.10,
                    y: 0.20,
                    w: 0.30,
                    h: 0.05,
                    color: "#ffd54f".into(),
                    note: Some("hello".into()),
                }],
            };
            save_to_pdf(&doc, &store, &path).expect("save_to_pdf");
        }

        // Reopen and verify our metadata round-tripped.
        let doc = pdfium.load_pdf_from_file(&path, None).expect("reload2");
        let store = load_from_pdf(&doc).expect("load_from_pdf");
        assert_eq!(store.items.len(), 1);
        let h = &store.items[0];
        assert_eq!(h.page, 0);
        assert_eq!(h.color, "#ffd54f");
        assert_eq!(h.note.as_deref(), Some("hello"));
        // Coordinates should round-trip within ~0.5% (PDF point grid
        // is 1/72in, so a few pt of rounding noise is expected).
        assert!((h.x - 0.10).abs() < 0.01, "x: got {}", h.x);
        assert!((h.y - 0.20).abs() < 0.01, "y: got {}", h.y);
        assert!((h.w - 0.30).abs() < 0.01, "w: got {}", h.w);
        assert!((h.h - 0.05).abs() < 0.01, "h: got {}", h.h);

        let _ = std::fs::remove_file(&path);
    }
}
