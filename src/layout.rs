//! Continuous-scroll page layout. Computes per-page pixel heights
//! and cumulative y-offsets so the renderer knows what to stitch
//! into the viewport at any scroll position.
//!
//! This module is intentionally pdfium-free: callers pass in a slice
//! of `PageMetrics` (cheap to query — width/height in points only,
//! no rasterisation) and get back a layout. That keeps the math
//! testable without a real PDF.

use std::ops::Range;

use crate::pdf::PageMetrics;

#[derive(Debug, Clone)]
pub struct PageLayout {
    /// Render width every page is rendered at, in pixels. All pages
    /// share a width; heights vary by per-page aspect ratio.
    pub fit_width_px: u32,
    /// Vertical gap between consecutive pages, in pixels.
    pub gap_px: u32,
    pub page_heights_px: Vec<u32>,
    /// `page_y_offsets[i]` = pixels from doc top to the top edge of
    /// page `i`. Same length as `page_heights_px`.
    pub page_y_offsets: Vec<i64>,
    /// Total document height including inter-page gaps but excluding
    /// the trailing gap after the last page.
    pub total_height_px: i64,
}

impl PageLayout {
    pub fn build(metrics: &[PageMetrics], fit_width_px: u32, gap_px: u32) -> Self {
        let mut heights = Vec::with_capacity(metrics.len());
        let mut offsets = Vec::with_capacity(metrics.len());
        let mut y: i64 = 0;
        for m in metrics {
            let h = if m.width_pts > 0.0 {
                ((fit_width_px as f32) * (m.height_pts / m.width_pts)).round() as u32
            } else {
                0
            };
            offsets.push(y);
            heights.push(h);
            y += h as i64 + gap_px as i64;
        }
        let total = if metrics.is_empty() {
            0
        } else {
            (y - gap_px as i64).max(0)
        };
        Self {
            fit_width_px,
            gap_px,
            page_heights_px: heights,
            page_y_offsets: offsets,
            total_height_px: total,
        }
    }

    /// Range of page indices (start..end, end exclusive) whose y-rect
    /// intersects the viewport. Empty if the document is empty or
    /// fully off-screen.
    pub fn visible_pages(&self, scroll_y_px: i64, viewport_h_px: u32) -> Range<usize> {
        if self.page_y_offsets.is_empty() {
            return 0..0;
        }
        let viewport_top = scroll_y_px;
        let viewport_bot = scroll_y_px + viewport_h_px as i64;
        let mut first: Option<usize> = None;
        let mut last_excl: usize = 0;
        for (i, &y) in self.page_y_offsets.iter().enumerate() {
            let h = self.page_heights_px[i] as i64;
            let page_top = y;
            let page_bot = y + h;
            if page_bot <= viewport_top {
                continue;
            }
            if page_top >= viewport_bot {
                break;
            }
            if first.is_none() {
                first = Some(i);
            }
            last_excl = i + 1;
        }
        match first {
            Some(f) => f..last_excl,
            None => 0..0,
        }
    }

    pub fn page_y(&self, idx: usize) -> i64 {
        self.page_y_offsets.get(idx).copied().unwrap_or(0)
    }

    pub fn page_h(&self, idx: usize) -> u32 {
        self.page_heights_px.get(idx).copied().unwrap_or(0)
    }

    /// Which page contains the given doc-y pixel. Falls back to the
    /// last page if past the end, or 0 if the doc is empty. Inter-
    /// page gaps count as belonging to the page above them.
    pub fn page_at(&self, y_px: i64) -> usize {
        if self.page_y_offsets.is_empty() {
            return 0;
        }
        if y_px < 0 {
            return 0;
        }
        for (i, &offset) in self.page_y_offsets.iter().enumerate() {
            let h = self.page_heights_px[i] as i64;
            // page i owns y ∈ [offset, offset + h + gap)
            if y_px < offset + h + self.gap_px as i64 {
                return i;
            }
        }
        self.page_y_offsets.len() - 1
    }

    /// Clamp a candidate scroll-y so it never sits past the end of
    /// the document. Negative values clamp to 0.
    pub fn clamp_scroll(&self, y: i64, viewport_h_px: u32) -> i64 {
        let max = (self.total_height_px - viewport_h_px as i64).max(0);
        y.clamp(0, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(sizes: &[(f32, f32)]) -> Vec<PageMetrics> {
        sizes
            .iter()
            .map(|&(w, h)| PageMetrics { width_pts: w, height_pts: h })
            .collect()
    }

    #[test]
    fn build_uniform_pages_sums_correctly() {
        let m = metrics(&[(100.0, 200.0), (100.0, 200.0), (100.0, 200.0)]);
        // fit_width = 50 → each page renders at 50×100. Gap 10.
        let l = PageLayout::build(&m, 50, 10);
        assert_eq!(l.page_heights_px, vec![100, 100, 100]);
        assert_eq!(l.page_y_offsets, vec![0, 110, 220]);
        // 3 pages * 100 + 2 gaps * 10 = 320.
        assert_eq!(l.total_height_px, 320);
    }

    #[test]
    fn build_empty_doc_is_zero_height() {
        let l = PageLayout::build(&[], 100, 8);
        assert_eq!(l.total_height_px, 0);
        assert!(l.page_heights_px.is_empty());
    }

    #[test]
    fn build_handles_landscape_and_portrait_mix() {
        // Page 0 portrait (1:2), page 1 landscape (2:1) — both
        // rendered at width=100 → heights 200 and 50.
        let m = metrics(&[(100.0, 200.0), (200.0, 100.0)]);
        let l = PageLayout::build(&m, 100, 0);
        assert_eq!(l.page_heights_px, vec![200, 50]);
        assert_eq!(l.total_height_px, 250);
    }

    #[test]
    fn visible_pages_at_top_returns_first_page_only() {
        let m = metrics(&[(100.0, 200.0); 5]);
        let l = PageLayout::build(&m, 50, 10);
        // viewport 50px starting at 0 → only page 0 visible.
        assert_eq!(l.visible_pages(0, 50), 0..1);
    }

    #[test]
    fn visible_pages_spans_two_pages_across_gap() {
        let m = metrics(&[(100.0, 200.0); 5]);
        let l = PageLayout::build(&m, 50, 10);
        // page 0: y ∈ [0,100); gap [100,110); page 1: [110, 210).
        // viewport [90, 130) → covers tail of page 0, gap, head of page 1.
        assert_eq!(l.visible_pages(90, 40), 0..2);
    }

    #[test]
    fn visible_pages_past_end_is_empty() {
        let m = metrics(&[(100.0, 200.0); 2]);
        let l = PageLayout::build(&m, 50, 10);
        // total = 100 + 10 + 100 = 210. viewport at 1000 → empty.
        assert_eq!(l.visible_pages(1000, 50), 0..0);
    }

    #[test]
    fn page_at_scroll_zero_is_first_page() {
        let m = metrics(&[(100.0, 200.0); 3]);
        let l = PageLayout::build(&m, 50, 10);
        assert_eq!(l.page_at(0), 0);
    }

    #[test]
    fn page_at_inside_gap_belongs_to_page_above() {
        let m = metrics(&[(100.0, 200.0); 3]);
        let l = PageLayout::build(&m, 50, 10);
        // page 0 ends at 100; gap [100,110). y=105 → page 0.
        assert_eq!(l.page_at(105), 0);
    }

    #[test]
    fn page_at_past_end_clamps_to_last_page() {
        let m = metrics(&[(100.0, 200.0); 3]);
        let l = PageLayout::build(&m, 50, 10);
        assert_eq!(l.page_at(99_999), 2);
    }

    #[test]
    fn clamp_scroll_caps_at_total_minus_viewport() {
        let m = metrics(&[(100.0, 200.0); 3]);
        let l = PageLayout::build(&m, 50, 10);
        // total = 320, viewport = 100 → max scroll = 220.
        assert_eq!(l.clamp_scroll(500, 100), 220);
        assert_eq!(l.clamp_scroll(-50, 100), 0);
    }

    #[test]
    fn clamp_scroll_when_doc_smaller_than_viewport() {
        let m = metrics(&[(100.0, 200.0)]);
        // single page 50×100, viewport 200 → can't scroll at all.
        let l = PageLayout::build(&m, 50, 10);
        assert_eq!(l.clamp_scroll(50, 200), 0);
    }
}
