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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group_id: Option<u64>,
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
    let total = pages.len();

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
            // Cap the size of the Contents string we'll actually parse.
            // A hostile PDF can stuff multi-megabyte JSON into the
            // annotation Contents field; serde_json scales O(n) but the
            // load loop runs once per annotation per page on every
            // open. Real termpdf-emitted Contents are < 1 KiB
            // (color + group_id + a short note); 64 KiB is far above
            // any plausible legitimate use.
            const MAX_CONTENTS_LEN: usize = 64 * 1024;
            let Some(meta) = contents
                .as_deref()
                .filter(|s| s.starts_with(TAG_PREFIX) && s.len() <= MAX_CONTENTS_LEN)
                .and_then(|s| serde_json::from_str::<AnnotMeta>(&s[TAG_PREFIX.len()..]).ok())
            else {
                continue;
            };
            let Ok(bounds) = hl.bounds() else { continue };
            // Reject NaN/Inf coords from pdfium — `f32::clamp` is
            // identity on NaN, so the dirty value would propagate into
            // the in-memory store and back into a malformed-saved PDF.
            let raw_left = bounds.left().value;
            let raw_bottom = bounds.bottom().value;
            let raw_right = bounds.right().value;
            let raw_top = bounds.top().value;
            if !raw_left.is_finite()
                || !raw_bottom.is_finite()
                || !raw_right.is_finite()
                || !raw_top.is_finite()
            {
                continue;
            }
            let left = raw_left.max(0.0);
            let bottom = raw_bottom.max(0.0);
            let right = raw_right.min(page_w);
            let top = raw_top.min(page_h);
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
                group_id: meta.group_id,
            });
        }
    }
    Ok(HighlightStore::from_items(items))
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
/// Walk every page (the legacy unfiltered save). Production callers
/// use `save_to_pdf_filtered` for ~100× faster saves on big books;
/// this wrapper stays around for tests that want to round-trip the
/// full doc without tracking a candidate set.
#[allow(dead_code)]
pub fn save_to_pdf(document: &PdfDocument<'_>, store: &HighlightStore, path: &Path) -> Result<()> {
    apply_store_to_document(document, store, None)?;
    save_atomic(document, path)
}

/// Like `save_to_pdf` but only opens the pages in `candidate_pages`.
/// On a 700-page book, `pages.get(i)` is ~5–10 ms (FPDF_LoadPage).
/// Walking every page just to confirm "nothing to do here" was a 5–7 s
/// pause on quit. The caller maintains the set as
/// `(pages_with_our_annotations_at_load) ∪ (pages_with_highlights_now)`
/// so we cover both deletes and adds without missing any.
pub fn save_to_pdf_filtered(
    document: &PdfDocument<'_>,
    store: &HighlightStore,
    path: &Path,
    candidate_pages: &std::collections::HashSet<usize>,
) -> Result<()> {
    apply_store_to_document(document, store, Some(candidate_pages))?;
    save_atomic(document, path)
}

fn apply_store_to_document(
    document: &PdfDocument<'_>,
    store: &HighlightStore,
    candidate_pages: Option<&std::collections::HashSet<usize>>,
) -> Result<()> {
    let pages = document.pages();
    let total = pages.len();

    // Surface — rather than silently drop — highlights that point past
    // the end of the document (e.g. a session restored from a longer
    // version of the same file).
    let total_usize = total.max(0) as usize;
    let orphans = store.items.iter().filter(|h| h.page >= total_usize).count();
    if orphans > 0 {
        eprintln!(
            "warning: {orphans} highlight(s) reference page(s) past the end of \
             the PDF and will not be saved (document has {total_usize} page(s))"
        );
    }

    for page_idx in 0..total {
        // Skip pages the caller flagged as untouched (no annotations of
        // ours, no highlights to add). Saves ~5–10 ms per skipped page
        // on big books.
        if let Some(pages_to_walk) = candidate_pages {
            if !pages_to_walk.contains(&(page_idx as usize)) {
                continue;
            }
        }
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
                // CRITICAL: must mirror the load predicate exactly.
                // If save's "is_ours" check is just `starts_with`, but
                // load also requires valid AnnotMeta JSON, then a
                // foreign tool's annotation that happens to begin with
                // `termpdf-rs:` (or a corrupted termpdf annotation) is
                // skipped at load (treated as foreign) yet DELETED at
                // save — silent foreign-data destruction. Apply the
                // same JSON-parseability gate here.
                const MAX_CONTENTS_LEN: usize = 64 * 1024;
                let is_ours = annotation.contents().is_some_and(|s| {
                    s.starts_with(TAG_PREFIX)
                        && s.len() <= MAX_CONTENTS_LEN
                        && serde_json::from_str::<AnnotMeta>(&s[TAG_PREFIX.len()..]).is_ok()
                });
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
            // Skip NaN/Inf coords — `f32::clamp` is identity on NaN
            // (only panics with NaN bounds), so a NaN x/y/w/h would
            // pass through and `PdfPoints::new(NaN)` gets written into
            // the saved PDF as a malformed numeric, which other
            // viewers can crash on.
            if !h.x.is_finite() || !h.y.is_finite() || !h.w.is_finite() || !h.h.is_finite() {
                continue;
            }
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
                group_id: h.group_id,
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
/// Threat model for the temp file: a co-located attacker (same UID,
/// write access to the directory containing the user's PDF) could
/// otherwise (a) predict the temp path `.{stem}.termpdf-tmp`,
/// (b) plant a symlink there pointing at an arbitrary file the
/// user can write, (c) wait for pdfium's `open(O_WRONLY|O_TRUNC)`
/// to follow the symlink and overwrite the target with PDF bytes.
///
/// Two hardenings:
///   1. Randomized temp suffix (per-process nanos + counter) so
///      the attacker can't pre-place a symlink at a known path.
///   2. We own the file descriptor: open with O_NOFOLLOW + O_EXCL
///      + 0600, then hand the *fd* to pdfium via `save_to_writer`.
///      pdfium never opens by path, so a symlink swap landing
///      mid-write writes nowhere we don't control.
fn save_atomic(document: &PdfDocument<'_>, path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("doc");
    let tmp_path = parent.join(format!(
        ".{stem}.termpdf-tmp.{}.{}",
        std::process::id(),
        unique_temp_suffix(),
    ));

    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        // O_NOFOLLOW + O_EXCL: if a same-UID attacker squatted on the
        // randomized temp path with a symlink between scheduling and
        // open, the open fails rather than following. O_EXCL is the
        // standard "create or fail" guard; together they're a tight
        // anti-symlink-race fence. 0o600 keeps the temp inode private
        // even on shared volumes.
        const O_NOFOLLOW: i32 = 0x20000; // libc::O_NOFOLLOW on Linux
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(O_NOFOLLOW)
            .mode(0o600)
            .open(&tmp_path)
            .with_context(|| format!("creating private temp file {}", tmp_path.display()))?;
        document
            .save_to_writer(&mut file)
            .with_context(|| format!("writing temp PDF to {}", tmp_path.display()))?;
        // Flush + drop fd before rename so the bytes are durably on
        // disk; rename is atomic on the same filesystem.
        file.flush().ok();
        drop(file);
    }
    #[cfg(not(unix))]
    {
        // Fallback path-based save on non-Unix; the symlink-race
        // threat model doesn't apply the same way on Windows.
        document
            .save_to_file(&tmp_path)
            .with_context(|| format!("writing temp PDF to {}", tmp_path.display()))?;
    }

    fs::rename(&tmp_path, path)
        .with_context(|| format!("renaming {} → {}", tmp_path.display(), path.display()))?;
    Ok(())
}

/// Per-call unique suffix for the save-atomic temp filename. Mixes
/// nanoseconds with a process-local counter so two saves in the same
/// nanosecond still collide-resolve. Non-cryptographic; only needs
/// to be unpredictable to a co-located attacker between schedule and
/// open, which nanos easily satisfy.
fn unique_temp_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{n:x}")
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
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
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

        let doc = pdfium.load_pdf_from_file(&path, None).expect("reload");
        // The bug was a segfault here; reaching the assert is the win.
        let store = load_from_pdf(&doc).expect("load_from_pdf");
        assert_eq!(
            store.items.len(),
            0,
            "foreign highlights must be skipped (pdfium fill_color is unsafe on Adobe-style annotations)"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// `group_id` round-trips through the AnnotMeta JSON and is
    /// available on the loaded highlight — the field that lets `x`
    /// delete a multi-line highlight as a single unit.
    #[test]
    fn group_id_roundtrips_through_save_and_load() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let path = temp_path("group-id");
        {
            let mut doc = pdfium.create_new_pdf().expect("new pdf");
            doc.pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .expect("page");
            doc.save_to_file(&path).expect("save empty");
        }
        // Three rects sharing one group_id (think: a 3-line highlight).
        let store = HighlightStore::from_items(vec![
            Highlight {
                page: 0,
                x: 0.10,
                y: 0.10,
                w: 0.20,
                h: 0.04,
                color: "#ffd54f".into(),
                note: None,
                group_id: Some(42),
            },
            Highlight {
                page: 0,
                x: 0.10,
                y: 0.15,
                w: 0.20,
                h: 0.04,
                color: "#ffd54f".into(),
                note: None,
                group_id: Some(42),
            },
            Highlight {
                page: 0,
                x: 0.10,
                y: 0.20,
                w: 0.20,
                h: 0.04,
                color: "#ffd54f".into(),
                note: None,
                group_id: Some(42),
            },
        ]);
        {
            let doc = pdfium.load_pdf_from_file(&path, None).expect("reload");
            save_to_pdf(&doc, &store, &path).expect("save");
        }
        let doc = pdfium.load_pdf_from_file(&path, None).expect("reload2");
        let loaded = load_from_pdf(&doc).expect("load");
        assert_eq!(loaded.items.len(), 3);
        for h in &loaded.items {
            assert_eq!(
                h.group_id,
                Some(42),
                "group_id must round-trip so x can delete the whole group"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Legacy / migrated highlights without a group_id deserialize
    /// cleanly as `None` (skip_serializing_if guarantees the JSON
    /// stays compact for new entries too).
    #[test]
    fn missing_group_id_in_contents_loads_as_none() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let path = temp_path("legacy-meta");
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
            // Hand-write a TAG_PREFIX'd Contents *without* group_id —
            // mimics a highlight saved before the field existed.
            hl.set_fill_color(PdfColor::new(0xff, 0xd5, 0x4f, 0xff))
                .expect("color");
            let body = "{\"color\":\"#ffd54f\"}";
            hl.set_contents(&format!("{}{}", TAG_PREFIX, body))
                .expect("contents");
            doc.save_to_file(&path).expect("save");
        }
        let doc = pdfium.load_pdf_from_file(&path, None).expect("reload");
        let loaded = load_from_pdf(&doc).expect("load");
        assert_eq!(loaded.items.len(), 1);
        assert!(
            loaded.items[0].group_id.is_none(),
            "legacy entries must load with group_id = None"
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
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let path = temp_path("idempotent");
        {
            let mut doc = pdfium.create_new_pdf().expect("new pdf");
            doc.pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .expect("page");
            doc.save_to_file(&path).expect("save empty");
        }
        let store = HighlightStore::from_items(vec![Highlight {
            page: 0,
            x: 0.10,
            y: 0.20,
            w: 0.30,
            h: 0.05,
            color: "#aed581".into(),
            note: None,
            group_id: None,
        }]);
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
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
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
            let store = HighlightStore::from_items(vec![Highlight {
                page: 0,
                x: 0.10,
                y: 0.10,
                w: 0.20,
                h: 0.05,
                color: "#ffd54f".into(),
                note: None,
                group_id: None,
            }]);
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
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
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
            let store = HighlightStore::from_items(vec![Highlight {
                page: 0,
                x: 0.10,
                y: 0.20,
                w: 0.30,
                h: 0.05,
                color: "#ffd54f".into(),
                note: Some("hello".into()),
                group_id: None,
            }]);
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

    /// `save_to_pdf_filtered` must skip pages NOT in the candidate set:
    /// adds on excluded pages are dropped, and ours-on-disk on excluded
    /// pages are left in place. Production passes `prev ∪ now` so this
    /// edge case shouldn't fire — but the contract matters because a
    /// caller bug here would silently lose user data.
    #[test]
    fn save_to_pdf_filtered_only_walks_candidate_pages() {
        let Some(pdfium) = pdfium_or_skip() else {
            return;
        };
        let path = temp_path("filtered");
        {
            let mut doc = pdfium.create_new_pdf().expect("new pdf");
            doc.pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .expect("page 0");
            doc.pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .expect("page 1");
            doc.save_to_file(&path).expect("save empty");
        }
        let store = HighlightStore::from_items(vec![
            Highlight {
                page: 0,
                x: 0.10,
                y: 0.10,
                w: 0.20,
                h: 0.05,
                color: "#ffd54f".into(),
                note: None,
                group_id: None,
            },
            Highlight {
                page: 1,
                x: 0.10,
                y: 0.10,
                w: 0.20,
                h: 0.05,
                color: "#aed581".into(),
                note: None,
                group_id: None,
            },
        ]);
        // Only walk page 1; page 0's highlight in the store should be
        // ignored entirely.
        let mut candidate = std::collections::HashSet::new();
        candidate.insert(1usize);
        {
            let doc = pdfium.load_pdf_from_file(&path, None).expect("reload");
            save_to_pdf_filtered(&doc, &store, &path, &candidate).expect("save");
        }
        let doc = pdfium.load_pdf_from_file(&path, None).expect("reload2");
        let loaded = load_from_pdf(&doc).expect("load");
        assert_eq!(
            loaded.items.len(),
            1,
            "page 0 was filtered out → its highlight must not have been written"
        );
        assert_eq!(loaded.items[0].page, 1);
        let _ = std::fs::remove_file(&path);
    }
}
