//! Text extraction from a PDF page given a normalised rectangle.
//! Bridges the visual coordinate space the UI works in (top-left
//! origin, 0..1 normalised) to pdfium's PDF point space (bottom-
//! left origin, in points).

use anyhow::{Context, Result};
use pdfium_render::prelude::*;

use crate::highlight::Rect01;
use crate::pdf::PageMetrics;

/// Extract the text contained inside `sel` (in normalised page
/// coords) on `page_idx`. Returns `Ok(String::new())` if the page
/// has no text characters in that region (image-only page,
/// scanned PDF, or selection over whitespace) — never an error
/// for "nothing to copy".
pub fn extract_rect(
    document: &PdfDocument<'_>,
    page_idx: usize,
    sel: Rect01,
    metrics: &PageMetrics,
) -> Result<String> {
    let pages = document.pages();
    let page = pages
        .get(page_idx as i32)
        .with_context(|| format!("loading page {} for text extraction", page_idx + 1))?;

    let pdf_rect = norm_to_pdf_rect(sel, metrics);
    let text = page
        .text()
        .with_context(|| format!("loading text for page {}", page_idx + 1))?;
    let raw = text.inside_rect(pdf_rect);

    // Strip stray CR + collapse Windows line endings — most PDFs are
    // fine but a few embed \r\n. Don't NFC-normalise; that would
    // alter author intent.
    let normalised = raw.replace("\r\n", "\n").replace('\r', "\n");
    Ok(normalised)
}

/// Convert a top-left-origin normalised Rect01 into a pdfium
/// bottom-left-origin PdfRect in points. PdfRect::new defensively
/// reorders if you pass the corners wrong, but the values would be
/// silently incorrect — so do the flip carefully here and let the
/// caller see the same `(x, y, w, h)` semantics they passed in.
fn norm_to_pdf_rect(sel: Rect01, metrics: &PageMetrics) -> PdfRect {
    let w = metrics.width_pts;
    let h = metrics.height_pts;
    let left = (sel.x.clamp(0.0, 1.0)) * w;
    let right = ((sel.x + sel.w).clamp(0.0, 1.0)) * w;
    // Y flip: top-left origin → bottom-left origin.
    let top = (1.0 - sel.y.clamp(0.0, 1.0)) * h;
    let bottom = (1.0 - (sel.y + sel.h).clamp(0.0, 1.0)) * h;
    PdfRect::new(
        PdfPoints::new(bottom),
        PdfPoints::new(left),
        PdfPoints::new(top),
        PdfPoints::new(right),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_to_pdf_rect_full_page() {
        let m = PageMetrics { width_pts: 100.0, height_pts: 200.0 };
        let r = norm_to_pdf_rect(
            Rect01 { x: 0.0, y: 0.0, w: 1.0, h: 1.0 },
            &m,
        );
        assert!((r.left().value - 0.0).abs() < 1e-3);
        assert!((r.right().value - 100.0).abs() < 1e-3);
        assert!((r.bottom().value - 0.0).abs() < 1e-3);
        assert!((r.top().value - 200.0).abs() < 1e-3);
    }

    #[test]
    fn norm_to_pdf_rect_top_quarter_flips_y() {
        // Top-left quarter of a 100×200 page, in UI coords:
        // x=0, y=0, w=0.5, h=0.5 → left=0, right=50, top=200, bottom=100.
        let m = PageMetrics { width_pts: 100.0, height_pts: 200.0 };
        let r = norm_to_pdf_rect(
            Rect01 { x: 0.0, y: 0.0, w: 0.5, h: 0.5 },
            &m,
        );
        assert!((r.left().value - 0.0).abs() < 1e-3);
        assert!((r.right().value - 50.0).abs() < 1e-3);
        assert!((r.top().value - 200.0).abs() < 1e-3);
        assert!((r.bottom().value - 100.0).abs() < 1e-3);
    }

    #[test]
    fn norm_to_pdf_rect_bottom_quarter() {
        // Bottom-left quarter in UI coords: x=0, y=0.5, w=0.5, h=0.5
        // → left=0, right=50, top=100, bottom=0.
        let m = PageMetrics { width_pts: 100.0, height_pts: 200.0 };
        let r = norm_to_pdf_rect(
            Rect01 { x: 0.0, y: 0.5, w: 0.5, h: 0.5 },
            &m,
        );
        assert!((r.top().value - 100.0).abs() < 1e-3);
        assert!((r.bottom().value - 0.0).abs() < 1e-3);
    }

    #[test]
    fn norm_to_pdf_rect_clamps_out_of_range() {
        // A drag past page edges shouldn't produce a degenerate rect.
        let m = PageMetrics { width_pts: 100.0, height_pts: 200.0 };
        let r = norm_to_pdf_rect(
            Rect01 { x: -0.5, y: -0.5, w: 5.0, h: 5.0 },
            &m,
        );
        assert!((r.left().value - 0.0).abs() < 1e-3);
        assert!((r.right().value - 100.0).abs() < 1e-3);
        assert!((r.bottom().value - 0.0).abs() < 1e-3);
        assert!((r.top().value - 200.0).abs() < 1e-3);
    }
}
