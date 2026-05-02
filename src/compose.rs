//! Pure pixel-space helpers shared by `ui::ensure_image` and the
//! tests. Kept separate from `ui.rs` so they can be exercised with
//! synthetic RgbaImages — the tests don't need pdfium or a real PDF.

use image::RgbaImage;

use crate::highlight::Rect01;

/// Convert a normalized 0..1 rectangle into pixel coordinates on an
/// image of size `(w, h)`. Saturating math keeps a slightly out-of-
/// range stored highlight from panicking on `crop_imm` later in the
/// pipeline.
pub fn norm_to_pixels(r: Rect01, w: u32, h: u32) -> (u32, u32, u32, u32) {
    let px = (r.x.clamp(0.0, 1.0) * w as f32) as u32;
    let py = (r.y.clamp(0.0, 1.0) * h as f32) as u32;
    let pw = ((r.w.clamp(0.0, 1.0) * w as f32) as u32).max(1);
    let ph = ((r.h.clamp(0.0, 1.0) * h as f32) as u32).max(1);
    (px.min(w.saturating_sub(1)), py.min(h.saturating_sub(1)), pw, ph)
}

/// Compute the (crop_x, crop_y, crop_w, crop_h) rectangle that cuts
/// the visible viewport out of a full-size pixmap. `scroll_*` is in
/// 0..=1; the result is clamped so scroll values just outside the
/// range still yield a valid crop.
///
/// Reserved for the per-page crop fast-path (a future zoom-aware
/// optimisation) and exercised by unit tests; current continuous
/// renderer uses `blit_clipped` instead.
#[allow(dead_code)]
pub fn crop_rect(
    full_w: u32,
    full_h: u32,
    viewport_w: u32,
    viewport_h: u32,
    scroll_x: f32,
    scroll_y: f32,
) -> (u32, u32, u32, u32) {
    let crop_w = full_w.min(viewport_w.max(1));
    let crop_h = full_h.min(viewport_h.max(1));
    let overflow_x = full_w.saturating_sub(crop_w) as f32;
    let overflow_y = full_h.saturating_sub(crop_h) as f32;
    let sx = scroll_x.clamp(0.0, 1.0);
    let sy = scroll_y.clamp(0.0, 1.0);
    let crop_x = (overflow_x * sx).round() as u32;
    let crop_y = (overflow_y * sy).round() as u32;
    (crop_x, crop_y, crop_w, crop_h)
}

pub fn fill_rect_blend(
    img: &mut RgbaImage,
    rect: (u32, u32, u32, u32),
    color: (u8, u8, u8),
    alpha: f32,
) {
    let (x, y, w, h) = rect;
    let img_w = img.width();
    let img_h = img.height();
    let x2 = (x + w).min(img_w);
    let y2 = (y + h).min(img_h);
    let a = alpha.clamp(0.0, 1.0);
    let (cr, cg, cb) = color;
    for py in y..y2 {
        for px in x..x2 {
            let p = img.get_pixel_mut(px, py);
            p.0[0] = (p.0[0] as f32 * (1.0 - a) + cr as f32 * a) as u8;
            p.0[1] = (p.0[1] as f32 * (1.0 - a) + cg as f32 * a) as u8;
            p.0[2] = (p.0[2] as f32 * (1.0 - a) + cb as f32 * a) as u8;
        }
    }
}

pub fn outline_rect(
    img: &mut RgbaImage,
    rect: (u32, u32, u32, u32),
    color: (u8, u8, u8),
    thickness: u32,
) {
    let (x, y, w, h) = rect;
    let t = thickness.max(1);
    fill_rect_blend(img, (x, y, w, t), color, 1.0);
    fill_rect_blend(img, (x, y + h.saturating_sub(t), w, t), color, 1.0);
    fill_rect_blend(img, (x, y, t, h), color, 1.0);
    fill_rect_blend(img, (x + w.saturating_sub(t), y, t, h), color, 1.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn white(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba([255, 255, 255, 255]))
    }

    #[test]
    fn norm_to_pixels_maps_centered_quarter() {
        // Quarter rectangle at (25%, 25%) sized 50%×50% on a 200×100 image
        // → starts at (50, 25), spans 100×50.
        let r = Rect01 { x: 0.25, y: 0.25, w: 0.5, h: 0.5 };
        let (x, y, w, h) = norm_to_pixels(r, 200, 100);
        assert_eq!((x, y, w, h), (50, 25, 100, 50));
    }

    #[test]
    fn norm_to_pixels_clamps_out_of_range() {
        // A negative-x highlight (legacy data corruption) must not panic
        // and must produce an in-bounds origin.
        let r = Rect01 { x: -0.2, y: 1.5, w: 2.0, h: 0.1 };
        let (x, y, w, _h) = norm_to_pixels(r, 100, 100);
        assert_eq!(x, 0);
        assert!(y < 100);
        assert_eq!(w, 100);
    }

    #[test]
    fn crop_rect_no_overflow_returns_full_image() {
        // Image fits inside the viewport → no crop, no scroll offset
        // even if scroll values are nonzero.
        let r = crop_rect(80, 60, 200, 100, 0.7, 0.3);
        assert_eq!(r, (0, 0, 80, 60));
    }

    #[test]
    fn crop_rect_at_scroll_zero_pins_to_top_left() {
        let r = crop_rect(400, 300, 200, 100, 0.0, 0.0);
        assert_eq!(r, (0, 0, 200, 100));
    }

    #[test]
    fn crop_rect_at_scroll_one_pins_to_bottom_right() {
        let r = crop_rect(400, 300, 200, 100, 1.0, 1.0);
        // Overflow is 200 horizontally, 200 vertically.
        assert_eq!(r, (200, 200, 200, 100));
    }

    #[test]
    fn crop_rect_midway_lands_in_middle() {
        let r = crop_rect(400, 300, 200, 100, 0.5, 0.5);
        assert_eq!(r, (100, 100, 200, 100));
    }

    #[test]
    fn fill_rect_blend_full_alpha_overwrites_pixels() {
        let mut img = white(10, 10);
        fill_rect_blend(&mut img, (2, 2, 4, 4), (255, 0, 0), 1.0);
        assert_eq!(img.get_pixel(3, 3).0, [255, 0, 0, 255]);
        // Outside the rect stays white.
        assert_eq!(img.get_pixel(0, 0).0, [255, 255, 255, 255]);
        assert_eq!(img.get_pixel(7, 7).0, [255, 255, 255, 255]);
    }

    #[test]
    fn fill_rect_blend_half_alpha_averages_with_background() {
        let mut img = white(4, 4);
        // Yellow on white at α=0.5 → channels move halfway from 255
        // toward (255, 213, 79) → ~(255, 234, 167).
        fill_rect_blend(&mut img, (0, 0, 4, 4), (255, 213, 79), 0.5);
        let p = img.get_pixel(1, 1).0;
        assert_eq!(p[0], 255);
        assert!((p[1] as i32 - 234).abs() <= 2, "g channel was {}", p[1]);
        assert!((p[2] as i32 - 167).abs() <= 2, "b channel was {}", p[2]);
        assert_eq!(p[3], 255);
    }

    #[test]
    fn fill_rect_blend_clips_at_image_edge() {
        let mut img = white(5, 5);
        // Rect extends past the right + bottom edges; must not panic.
        fill_rect_blend(&mut img, (3, 3, 10, 10), (0, 0, 0), 1.0);
        assert_eq!(img.get_pixel(4, 4).0, [0, 0, 0, 255]);
    }

    #[test]
    fn outline_rect_paints_only_borders() {
        let mut img = white(10, 10);
        outline_rect(&mut img, (1, 1, 8, 8), (0, 0, 0), 1);
        // Border pixel: black.
        assert_eq!(img.get_pixel(1, 1).0, [0, 0, 0, 255]);
        assert_eq!(img.get_pixel(8, 8).0, [0, 0, 0, 255]);
        // Interior of the outlined rect: still white.
        assert_eq!(img.get_pixel(4, 4).0, [255, 255, 255, 255]);
        // Outside the outlined rect: still white.
        assert_eq!(img.get_pixel(0, 0).0, [255, 255, 255, 255]);
    }
}
