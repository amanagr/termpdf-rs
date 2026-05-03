//! Full-text search over the open PDF.
//!
//! Strategy: run synchronously on Enter, walk every page through
//! pdfium's `PdfPageText::search`, collect every match's bounding
//! rect (one per visual line — pdfium merges adjacent characters
//! into segments). Store hits in normalised PDF page coords (0..1,
//! top-left origin) so the overlay code in `ui` can reuse the same
//! `norm_to_pixels` math used for highlights.
//!
//! For the typical corpus (papers, RFCs, docs under ~200 pages)
//! this completes in well under a second. Larger or scanned PDFs
//! get a future "budget N pages per frame tick" pass; v1 keeps it
//! simple.

use anyhow::Result;
use pdfium_render::prelude::*;

use crate::highlight::Rect01;
use crate::pdf::PageMetrics;

/// Hard cap on total search hits in one query. A query like "e" on a
/// 1000-page book would otherwise allocate millions of `SearchHit`
/// entries — both a UX problem (can't navigate them) and a memory-
/// pressure / DoS surface against a hostile PDF. We stop collecting
/// once the cap is reached and surface a hint in the status line.
const MAX_SEARCH_HITS: usize = 5_000;

#[derive(Debug, Clone, Copy)]
pub struct SearchHit {
    pub page: usize,
    pub rect: Rect01,
}

#[derive(Debug, Clone)]
pub struct SearchResults {
    pub query: String,
    pub hits: Vec<SearchHit>,
    /// Index into `hits` of the currently-focused match.
    pub current: usize,
    /// Bumped on every search state change so the compose cache can
    /// detect "user advanced n" without diffing the hits vector.
    pub revision: u64,
}

impl SearchResults {
    pub fn empty(query: String) -> Self {
        Self {
            query,
            hits: Vec::new(),
            current: 0,
            revision: 0,
        }
    }

    pub fn current_hit(&self) -> Option<&SearchHit> {
        self.hits.get(self.current)
    }

    pub fn advance(&mut self, dir: i32) {
        if self.hits.is_empty() {
            return;
        }
        let n = self.hits.len() as i32;
        let next = ((self.current as i32) + dir).rem_euclid(n);
        self.current = next as usize;
        self.revision = self.revision.wrapping_add(1);
    }
}

/// Run a full-document search. Errors only on pdfium failures —
/// "no matches" returns `Ok(SearchResults { hits: empty })`.
///
/// Optimisation: when `index` is provided, use it as a per-page
/// filter — only scan pages whose indexed text contains the query.
/// On a doc with the index fully built, an "xyz" query matching
/// 5 of 700 pages does 5 pdfium scans instead of 700. Pages
/// outside the indexed prefix still need pdfium (worst case: same
/// as the no-index path). Pass `None` to disable the optimisation.
pub fn run_search(
    document: &PdfDocument<'_>,
    page_metrics: &[PageMetrics],
    query: &str,
    case_sensitive: bool,
    index: Option<&crate::search_index::DocIndex>,
) -> Result<SearchResults> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(SearchResults::empty(query.to_string()));
    }

    // Build the candidate page set. With a complete index, this is
    // exactly the set of pages whose text contains the query — we
    // skip pdfium entirely for non-matching pages. With a partial
    // index, indexed-non-matching pages get skipped and unindexed
    // pages get scanned (pdfium-authoritative on those).
    let candidates: Vec<usize> = match index {
        Some(idx) => {
            let mut hit_pages: Vec<usize> = idx.pages_matching(trimmed, case_sensitive);
            if !idx.is_complete() {
                // Augment with all unindexed pages — we don't yet
                // know whether they match.
                for p in 0..page_metrics.len() {
                    if !idx.contains(p) {
                        hit_pages.push(p);
                    }
                }
                hit_pages.sort_unstable();
                hit_pages.dedup();
            }
            hit_pages
        }
        None => (0..page_metrics.len()).collect(),
    };

    let mut hits = Vec::new();
    let pages = document.pages();
    let opts = PdfSearchOptions::new().match_case(case_sensitive);

    'pages: for page_idx in candidates {
        let Some(m) = page_metrics.get(page_idx) else {
            continue;
        };
        let Ok(page) = pages.get(page_idx as i32) else {
            continue;
        };
        let Ok(text) = page.text() else { continue };
        let Ok(search) = text.search(trimmed, &opts) else { continue };
        for match_segments in search.iter(PdfSearchDirection::SearchForward) {
            for seg in match_segments.iter() {
                if hits.len() >= MAX_SEARCH_HITS {
                    break 'pages;
                }
                let r = seg.bounds();
                hits.push(SearchHit {
                    page: page_idx,
                    rect: pdf_rect_to_norm(&r, m),
                });
            }
        }
    }

    Ok(SearchResults {
        query: trimmed.to_string(),
        hits,
        current: 0,
        revision: 1,
    })
}

/// PDF point space (origin bottom-left, units = points) → normalised
/// 0..1 (origin top-left). `metrics` gives the page's natural
/// dimensions in points; the rendered bitmap and layout heights
/// derive from the same aspect ratio so a single sx factor works
/// for both axes.
fn pdf_rect_to_norm(r: &PdfRect, m: &PageMetrics) -> Rect01 {
    let w = m.width_pts.max(1.0);
    let h = m.height_pts.max(1.0);
    // Some pages report rotated rects where left > right or
    // bottom > top (pdfium leaves the raw text-frame coords on
    // rotated pages). Use min/max so the resulting normalised
    // rect always has positive width/height instead of clamping
    // a negative value to zero and dropping the hit visually.
    let raw_left = r.left().value;
    let raw_right = r.right().value;
    let raw_top = r.top().value;
    let raw_bottom = r.bottom().value;
    let left = raw_left.min(raw_right);
    let right = raw_left.max(raw_right);
    let top = raw_top.max(raw_bottom);
    let bottom = raw_top.min(raw_bottom);
    Rect01 {
        x: (left / w).clamp(0.0, 1.0),
        y: ((h - top) / h).clamp(0.0, 1.0),
        w: ((right - left) / w).clamp(0.0, 1.0),
        h: ((top - bottom) / h).clamp(0.0, 1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_returns_empty_results() {
        // Build a fake "results" without hitting pdfium.
        let r = SearchResults::empty("".into());
        assert!(r.hits.is_empty());
        assert_eq!(r.current, 0);
    }

    #[test]
    fn advance_wraps_around() {
        let mut r = SearchResults {
            query: "x".into(),
            hits: vec![
                SearchHit { page: 0, rect: Rect01 { x: 0.0, y: 0.0, w: 0.1, h: 0.1 } },
                SearchHit { page: 1, rect: Rect01 { x: 0.0, y: 0.0, w: 0.1, h: 0.1 } },
                SearchHit { page: 2, rect: Rect01 { x: 0.0, y: 0.0, w: 0.1, h: 0.1 } },
            ],
            current: 0,
            revision: 0,
        };
        r.advance(1);
        assert_eq!(r.current, 1);
        r.advance(1);
        r.advance(1);
        // Wraps from 2 → 0.
        assert_eq!(r.current, 0);
        // Backwards wraps from 0 → 2.
        r.advance(-1);
        assert_eq!(r.current, 2);
    }

    #[test]
    fn advance_on_empty_is_noop() {
        let mut r = SearchResults::empty("x".into());
        r.advance(1);
        assert_eq!(r.current, 0);
        r.advance(-1);
        assert_eq!(r.current, 0);
    }

    #[test]
    fn advance_bumps_revision_only_when_hits_exist() {
        let mut r = SearchResults {
            query: "x".into(),
            hits: vec![SearchHit { page: 0, rect: Rect01 { x: 0.0, y: 0.0, w: 0.1, h: 0.1 } }; 2],
            current: 0,
            revision: 5,
        };
        r.advance(1);
        assert_eq!(r.revision, 6);
        let mut r2 = SearchResults::empty("x".into());
        r2.revision = 7;
        r2.advance(1);
        assert_eq!(r2.revision, 7);
    }

    #[test]
    fn pdf_rect_to_norm_top_strip() {
        // A rect at the top of a 100×200pt page: left=10, right=90,
        // top=200, bottom=180 → normalised x=0.1, w=0.8, y=0.0,
        // h=0.10 (the top 10% of the page).
        let m = PageMetrics { width_pts: 100.0, height_pts: 200.0 };
        let r = PdfRect::new(
            PdfPoints::new(180.0),
            PdfPoints::new(10.0),
            PdfPoints::new(200.0),
            PdfPoints::new(90.0),
        );
        let n = pdf_rect_to_norm(&r, &m);
        assert!((n.x - 0.1).abs() < 1e-3, "x was {}", n.x);
        assert!((n.w - 0.8).abs() < 1e-3, "w was {}", n.w);
        assert!((n.y - 0.0).abs() < 1e-3, "y was {}", n.y);
        assert!((n.h - 0.10).abs() < 1e-3, "h was {}", n.h);
    }
}
