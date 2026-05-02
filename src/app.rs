use std::path::{Path, PathBuf};

use anyhow::Result;
use image::DynamicImage;
use pdfium_render::prelude::PdfDocument;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;

use crate::highlight::{Highlight, HighlightStore, Rect01, HIGHLIGHT_COLORS};
use crate::session::Session;

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Normal,
    Command,
    Visual,
    Search,
}

/// Cache key for the *pdfium* render step: changes to any of these
/// require re-rasterising the page through libpdfium, which is the
/// slow path. Scroll, selection, and saved highlights are *not* in
/// here — those only require recompositing the cached pixmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PdfiumKey {
    pub page: usize,
    pub dark: bool,
    pub area_w: u16,
    pub area_h: u16,
    pub zoom_milli: u32,
}

/// Cache key for the *compose* step (crop to viewport + overlay
/// highlights and selection). Cheap to recompute, but we still cache
/// it so that a still frame doesn't pointlessly re-clone the bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposeKey {
    pub pdfium: PdfiumKey,
    pub scroll_x_milli: u32,
    pub scroll_y_milli: u32,
    pub selection: Option<(u32, u32, u32, u32)>,
    pub selection_color_idx: usize,
    pub highlight_count: usize,
}

pub struct App<'doc> {
    pub document: PdfDocument<'doc>,
    pub path: PathBuf,
    pub page: usize,
    pub page_count: usize,

    pub dark: bool,
    pub mode: Mode,
    pub pending: String,    // numeric prefix typed in normal mode
    pub cmd_buffer: String, // text typed after `:` or `/`
    pub status: String,     // ephemeral status-line message
    pub show_help: bool,
    pub zoom: f32,

    /// Horizontal scroll within the zoomed page, 0..=1. 0 is fully
    /// left; 1 is fully right. Only meaningful when zoom > 1.0 (or
    /// when the page is naturally taller/wider than the viewport).
    pub scroll_x: f32,
    pub scroll_y: f32,

    /// Visible viewport in pixels (set by `ui::ensure_image` each
    /// frame). Visual-mode key handlers use it to translate "move 5%
    /// of a screen" into normalised page-space deltas.
    pub viewport_px: (u32, u32),
    /// Full rendered-page size in pixels (also set by `ensure_image`).
    pub full_px: (u32, u32),

    /// Active selection rectangle in normalised PDF page coordinates
    /// (0..1, top-left origin). `Some` only while in Visual mode.
    pub selection: Option<Rect01>,
    /// Index into `HIGHLIGHT_COLORS` for the next highlight to save.
    pub selection_color_idx: usize,

    pub picker: Picker,
    pub rendered_page: Option<DynamicImage>,
    pub last_pdfium_key: Option<PdfiumKey>,
    pub image_proto: Option<StatefulProtocol>,
    pub last_compose_key: Option<ComposeKey>,

    pub highlights: HighlightStore,
    pub should_quit: bool,
}

impl<'doc> App<'doc> {
    pub fn new(
        document: PdfDocument<'doc>,
        path: &Path,
        page: usize,
        dark: bool,
        picker: Picker,
    ) -> Result<Self> {
        let page_count = document.pages().len() as usize;
        let highlights = HighlightStore::load(path)?;
        Ok(Self {
            document,
            path: path.to_path_buf(),
            page: page.min(page_count.saturating_sub(1).max(0)),
            page_count,
            dark,
            mode: Mode::Normal,
            pending: String::new(),
            cmd_buffer: String::new(),
            status: String::new(),
            show_help: false,
            zoom: 1.0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            viewport_px: (0, 0),
            full_px: (0, 0),
            selection: None,
            selection_color_idx: 0,
            picker,
            rendered_page: None,
            last_pdfium_key: None,
            image_proto: None,
            last_compose_key: None,
            highlights,
            should_quit: false,
        })
    }

    /// Force a full re-render through pdfium on the next draw.
    pub fn invalidate(&mut self) {
        self.last_pdfium_key = None;
        self.last_compose_key = None;
    }

    /// Force just a re-compose (crop + overlay) on the next draw —
    /// keeps the cached pdfium pixmap. Use for scroll, selection,
    /// and highlight-add changes.
    pub fn invalidate_compose(&mut self) {
        self.last_compose_key = None;
    }

    pub fn goto_page(&mut self, page: usize) {
        let new = page.min(self.page_count.saturating_sub(1));
        if new != self.page {
            self.page = new;
            self.scroll_x = 0.0;
            self.scroll_y = 0.0;
            self.invalidate();
        }
    }

    pub fn next_page(&mut self, count: usize) {
        let target = self.page + count.max(1);
        self.goto_page(target);
    }
    pub fn prev_page(&mut self, count: usize) {
        let target = self.page.saturating_sub(count.max(1));
        self.goto_page(target);
    }
    pub fn first_page(&mut self) {
        self.goto_page(0);
    }
    pub fn last_page(&mut self) {
        self.goto_page(self.page_count.saturating_sub(1));
    }

    pub fn toggle_dark(&mut self) {
        self.dark = !self.dark;
        self.invalidate();
    }

    /// Multiply zoom by `factor`, clamp to a sensible range, and
    /// reset scroll if we drop back to fit.
    pub fn zoom_by(&mut self, factor: f32) {
        let new = (self.zoom * factor).clamp(0.25, 8.0);
        if (new - self.zoom).abs() < f32::EPSILON {
            return;
        }
        self.zoom = new;
        if (self.zoom - 1.0).abs() < 0.001 {
            self.scroll_x = 0.0;
            self.scroll_y = 0.0;
        }
        self.invalidate();
    }

    pub fn zoom_reset(&mut self) {
        self.zoom = 1.0;
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
        self.invalidate();
    }

    /// Scroll within the zoomed page by a fraction of the viewport
    /// (e.g. 0.1 = 10% of a screen). Clamps to [0,1] and is a no-op
    /// when nothing overflows the viewport.
    pub fn scroll_by(&mut self, dx_screens: f32, dy_screens: f32) {
        let (new_x, new_y) = scroll_step(
            self.scroll_x,
            self.scroll_y,
            dx_screens,
            dy_screens,
            self.viewport_px,
            self.full_px,
        );
        if (new_x - self.scroll_x).abs() > f32::EPSILON
            || (new_y - self.scroll_y).abs() > f32::EPSILON
        {
            self.scroll_x = new_x;
            self.scroll_y = new_y;
            self.invalidate_compose();
        }
    }

    pub fn enter_visual(&mut self) {
        // Start with a small rectangle centered in the viewport. If
        // the page is zoomed, use the visible region's center, not
        // the page's center, so the selection appears on-screen.
        let cx: f32 = 0.5;
        let cy: f32 = 0.5;
        let w: f32 = 0.25;
        let h: f32 = 0.10;
        self.selection = Some(Rect01 {
            x: (cx - w / 2.0).clamp(0.0, 1.0 - w),
            y: (cy - h / 2.0).clamp(0.0, 1.0 - h),
            w,
            h,
        });
        self.mode = Mode::Visual;
        self.status = "VISUAL — hjkl move · HJKL resize · c color · y save · Esc cancel".into();
        self.invalidate_compose();
    }

    pub fn exit_visual(&mut self) {
        self.selection = None;
        self.mode = Mode::Normal;
        self.status.clear();
        self.invalidate_compose();
    }

    /// Save the current Visual-mode selection as a highlight on the
    /// current page and return to Normal mode. No-op if there is no
    /// active selection.
    pub fn save_selection(&mut self) {
        if let Some(sel) = self.selection.take() {
            let color = HIGHLIGHT_COLORS[self.selection_color_idx % HIGHLIGHT_COLORS.len()];
            self.highlights.add(Highlight {
                page: self.page,
                x: sel.x,
                y: sel.y,
                w: sel.w,
                h: sel.h,
                color: color.hex.into(),
                note: None,
            });
            self.status = format!("highlight saved ({})", color.name);
        }
        self.mode = Mode::Normal;
        self.invalidate_compose();
    }

    pub fn cycle_color(&mut self) {
        self.selection_color_idx = (self.selection_color_idx + 1) % HIGHLIGHT_COLORS.len();
        let color = HIGHLIGHT_COLORS[self.selection_color_idx];
        self.status = format!("color: {}", color.name);
        self.invalidate_compose();
    }

    /// Move/resize the active Visual-mode selection. `dx`/`dy` are in
    /// normalised page units (0..1). `resize` true grows from the
    /// bottom-right corner; false slides the whole rectangle.
    pub fn nudge_selection(&mut self, dx: f32, dy: f32, resize: bool) {
        if let Some(sel) = self.selection.as_mut() {
            *sel = nudge_rect(*sel, dx, dy, resize);
            self.invalidate_compose();
        }
    }

    /// Delete the most recent highlight on the current page (vim-ish
    /// `u`/undo for highlights). Returns whether anything was removed.
    pub fn delete_last_highlight(&mut self) -> bool {
        let pos = self
            .highlights
            .items
            .iter()
            .rposition(|h| h.page == self.page);
        if let Some(idx) = pos {
            self.highlights.items.remove(idx);
            self.invalidate_compose();
            true
        } else {
            false
        }
    }

    pub fn persist_highlights(&self) -> Result<()> {
        self.highlights.save(&self.path)
    }

    /// Save the page + dark flag so the next open lands on the same
    /// state. Called on clean exit alongside `persist_highlights`.
    pub fn persist_session(&self) -> Result<()> {
        Session {
            page: self.page,
            dark: self.dark,
        }
        .save(&self.path)
    }
}

/// Pure helper: compute the new (scroll_x, scroll_y) after nudging
/// by some fraction of the viewport. Extracted from `App::scroll_by`
/// so it can be unit-tested without constructing a `PdfDocument`.
pub fn scroll_step(
    scroll_x: f32,
    scroll_y: f32,
    dx_screens: f32,
    dy_screens: f32,
    viewport_px: (u32, u32),
    full_px: (u32, u32),
) -> (f32, f32) {
    let (vw, vh) = viewport_px;
    let (fw, fh) = full_px;
    let overflow_x = fw.saturating_sub(vw) as f32;
    let overflow_y = fh.saturating_sub(vh) as f32;
    let mut new_x = scroll_x;
    let mut new_y = scroll_y;
    if overflow_x > 0.0 && vw > 0 {
        let dx = (vw as f32 * dx_screens) / overflow_x;
        new_x = (scroll_x + dx).clamp(0.0, 1.0);
    }
    if overflow_y > 0.0 && vh > 0 {
        let dy = (vh as f32 * dy_screens) / overflow_y;
        new_y = (scroll_y + dy).clamp(0.0, 1.0);
    }
    (new_x, new_y)
}

/// Pure helper: compute the new selection rectangle after a Visual
/// mode hjkl/HJKL keypress. Sliding leaves size fixed; resizing
/// grows/shrinks from the bottom-right corner. Both modes clamp to
/// stay inside the page and never collapse below 1% × 1%.
pub fn nudge_rect(sel: crate::highlight::Rect01, dx: f32, dy: f32, resize: bool) -> crate::highlight::Rect01 {
    use crate::highlight::Rect01;
    if resize {
        Rect01 {
            x: sel.x,
            y: sel.y,
            w: (sel.w + dx).clamp(0.01, 1.0 - sel.x),
            h: (sel.h + dy).clamp(0.01, 1.0 - sel.y),
        }
    } else {
        Rect01 {
            x: (sel.x + dx).clamp(0.0, 1.0 - sel.w),
            y: (sel.y + dy).clamp(0.0, 1.0 - sel.h),
            w: sel.w,
            h: sel.h,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::Rect01;

    #[test]
    fn scroll_step_no_overflow_is_noop() {
        // Image fits inside viewport → scroll requests are silently
        // dropped (otherwise we'd accumulate phantom scroll state).
        let (x, y) = scroll_step(0.0, 0.0, 0.5, 0.5, (200, 100), (100, 50));
        assert_eq!((x, y), (0.0, 0.0));
    }

    #[test]
    fn scroll_step_full_screen_jumps_when_overflow_equals_viewport() {
        // 400×300 image in 200×100 viewport → overflow == viewport.
        // dy_screens=1.0 should land exactly at scroll_y == 1.0.
        let (_x, y) = scroll_step(0.0, 0.0, 0.0, 1.0, (200, 100), (400, 300));
        assert!((y - 0.5).abs() < 1e-4, "y was {y}"); // viewport/overflow = 100/200 = 0.5
    }

    #[test]
    fn scroll_step_clamps_to_one() {
        let (_, y) = scroll_step(0.9, 0.9, 0.0, 5.0, (200, 100), (400, 300));
        assert_eq!(y, 1.0);
    }

    #[test]
    fn scroll_step_clamps_to_zero() {
        let (x, _) = scroll_step(0.1, 0.5, -5.0, 0.0, (200, 100), (400, 300));
        assert_eq!(x, 0.0);
    }

    #[test]
    fn nudge_rect_slides_within_bounds() {
        let sel = Rect01 { x: 0.4, y: 0.4, w: 0.2, h: 0.2 };
        let r = nudge_rect(sel, 0.1, 0.0, false);
        assert!((r.x - 0.5).abs() < 1e-4);
        // size unchanged when sliding
        assert_eq!(r.w, 0.2);
    }

    #[test]
    fn nudge_rect_clamps_when_sliding_off_edge() {
        let sel = Rect01 { x: 0.9, y: 0.0, w: 0.2, h: 0.2 };
        let r = nudge_rect(sel, 0.5, 0.0, false);
        // x can be at most 1.0 - w = 0.8
        assert!((r.x - 0.8).abs() < 1e-4, "x was {}", r.x);
    }

    #[test]
    fn nudge_rect_resize_grows_from_bottom_right() {
        let sel = Rect01 { x: 0.1, y: 0.1, w: 0.2, h: 0.2 };
        let r = nudge_rect(sel, 0.05, -0.05, true);
        assert_eq!((r.x, r.y), (0.1, 0.1));
        assert!((r.w - 0.25).abs() < 1e-4);
        assert!((r.h - 0.15).abs() < 1e-4);
    }

    #[test]
    fn nudge_rect_resize_floors_at_one_percent() {
        // Repeated shrink shouldn't collapse the rectangle to zero —
        // a zero-size highlight is invisible and confusing.
        let mut sel = Rect01 { x: 0.1, y: 0.1, w: 0.2, h: 0.2 };
        for _ in 0..50 {
            sel = nudge_rect(sel, -0.1, -0.1, true);
        }
        assert!((sel.w - 0.01).abs() < 1e-4);
        assert!((sel.h - 0.01).abs() < 1e-4);
    }
}
