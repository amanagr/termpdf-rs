use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use image::DynamicImage;
use pdfium_render::prelude::PdfDocument;
use ratatui::layout::Rect;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;

use crate::highlight::{Highlight, HighlightStore, Rect01, HIGHLIGHT_COLORS};
use crate::layout::PageLayout;
use crate::pdf::{self, PageMetrics};
use crate::session::Session;

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Normal,
    Command,
    Visual,
    Search,
}

/// Cache key for the *layout + per-page render* tier. When any of
/// these changes, every cached page bitmap is stale and the layout
/// must be rebuilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutKey {
    pub fit_width_px: u32,
    pub dark: bool,
}

/// Cache key for the *compose* tier (stitch visible pages into a
/// viewport-sized canvas, blend overlays). Cheap; we still cache it
/// so a still frame doesn't pointlessly re-blit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposeKey {
    pub layout: LayoutKey,
    pub viewport_w: u32,
    pub viewport_h: u32,
    pub scroll_y_px: i64,
    pub scroll_x_milli: u32,
    pub selection: Option<(usize, u32, u32, u32, u32)>,
    pub selection_color_idx: usize,
    pub highlight_revision: u64,
}

pub struct App<'doc> {
    pub document: PdfDocument<'doc>,
    pub path: PathBuf,
    pub page_count: usize,
    pub page_metrics: Vec<PageMetrics>,

    pub dark: bool,
    pub mode: Mode,
    pub pending: String,    // numeric prefix typed in normal mode
    pub cmd_buffer: String, // text typed after `:` or `/`
    pub status: String,     // ephemeral status-line message
    pub show_help: bool,
    pub zoom: f32,

    /// Vertical scroll position in pixels from the top of the
    /// continuous document. `0` is doc-top; clamped at
    /// `total_height - viewport_h`.
    pub scroll_y_px: i64,
    /// Horizontal scroll, 0..=1 over (fit_width_px - viewport_w_px).
    /// Only meaningful when zoom > 1 (page wider than viewport).
    pub scroll_x: f32,

    /// Set every frame by `ui::ensure_image`. Visual-mode key handler
    /// uses these to convert "screen-fraction" deltas into
    /// page-relative ones, and `dispatch_mouse` uses them to map
    /// terminal cells back to page coordinates.
    pub viewport_px: (u32, u32),
    pub image_area: Rect,
    pub cell_size_px: (u16, u16),
    /// Current `PageLayout` matching the cached page bitmaps. Built
    /// from `page_metrics` whenever `LayoutKey` changes.
    pub layout: PageLayout,
    pub last_layout_key: Option<LayoutKey>,

    /// Active selection rectangle in normalised PDF page coords
    /// (0..1, top-left origin), bound to a specific page.
    pub selection: Option<Rect01>,
    pub selection_page: usize,
    pub selection_color_idx: usize,
    /// Mouse-drag anchor point in normalised page coords; set on
    /// left-click, updated on drag, consumed on release.
    pub drag_anchor: Option<(usize, f32, f32)>,

    pub picker: Picker,
    /// Per-page rendered bitmap (no overlays applied). Sliding-
    /// window evicted around the visible page range so memory stays
    /// bounded on large documents.
    pub page_cache: HashMap<usize, DynamicImage>,
    pub image_proto: Option<StatefulProtocol>,
    pub last_compose_key: Option<ComposeKey>,

    pub highlights: HighlightStore,
    /// Bumped on every highlight add/delete so the compose cache
    /// invalidates without re-hashing the store.
    pub highlight_revision: u64,
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
        let page_metrics = pdf::page_metrics(&document)?;
        let highlights = HighlightStore::load(path)?;
        // Empty layout — first `ensure_image` call builds a real one
        // once the viewport size is known.
        let layout = PageLayout::build(&[], 0, 0);
        let mut app = Self {
            document,
            path: path.to_path_buf(),
            page_count,
            page_metrics,
            dark,
            mode: Mode::Normal,
            pending: String::new(),
            cmd_buffer: String::new(),
            status: String::new(),
            show_help: false,
            zoom: 1.0,
            scroll_y_px: 0,
            scroll_x: 0.0,
            viewport_px: (0, 0),
            image_area: Rect::default(),
            cell_size_px: picker.font_size(),
            layout,
            last_layout_key: None,
            selection: None,
            selection_page: 0,
            selection_color_idx: 0,
            drag_anchor: None,
            picker,
            page_cache: HashMap::new(),
            image_proto: None,
            last_compose_key: None,
            highlights,
            highlight_revision: 0,
            should_quit: false,
        };
        // Restore initial scroll position from the saved page (if any).
        // The actual y-pixel offset is computed once `layout` is built
        // for real on the first frame; for now stash the requested
        // page in `selection_page` as the "wanted page" so we don't
        // need an extra field. We resolve in `ensure_layout`.
        app.scroll_y_px = page as i64; // sentinel: < page_count means "page index, not pixels"
        Ok(app)
    }

    /// Rebuild the layout if `LayoutKey` changed. Resolves the
    /// startup "page-as-sentinel" trick into a real pixel offset on
    /// the first call. Called at the top of `ui::ensure_image`.
    pub fn ensure_layout(&mut self, fit_width_px: u32, viewport_h_px: u32) {
        let key = LayoutKey {
            fit_width_px,
            dark: self.dark,
        };
        if self.last_layout_key == Some(key) {
            // Layout unchanged — but viewport_h might have shrunk so
            // the current scroll could exceed the new max. Re-clamp.
            self.scroll_y_px = self.layout.clamp_scroll(self.scroll_y_px, viewport_h_px);
            return;
        }
        // Translate the startup sentinel (small i64 == page index)
        // before we lose the old layout. After the first frame
        // scroll_y_px is always a real pixel offset.
        let want_page_jump = if self.last_layout_key.is_none() {
            let page_idx = self.scroll_y_px as usize;
            if page_idx < self.page_count {
                Some(page_idx)
            } else {
                None
            }
        } else {
            // Preserve current scroll *as a fraction of the page we're
            // looking at* across zoom changes — otherwise zooming in
            // jumps the user back to the doc top.
            let cur_page = self.layout.page_at(self.scroll_y_px);
            let cur_page_y = self.layout.page_y(cur_page);
            let cur_page_h = self.layout.page_h(cur_page).max(1) as f32;
            let frac_into_page = (self.scroll_y_px - cur_page_y) as f32 / cur_page_h;
            self.layout = PageLayout::build(&self.page_metrics, fit_width_px, layout_gap_px());
            let new_page_y = self.layout.page_y(cur_page);
            let new_page_h = self.layout.page_h(cur_page) as f32;
            self.scroll_y_px = new_page_y + (frac_into_page * new_page_h) as i64;
            self.scroll_y_px = self.layout.clamp_scroll(self.scroll_y_px, viewport_h_px);
            self.last_layout_key = Some(key);
            self.page_cache.clear();
            self.image_proto = None;
            self.last_compose_key = None;
            return;
        };

        self.layout = PageLayout::build(&self.page_metrics, fit_width_px, layout_gap_px());
        if let Some(page_idx) = want_page_jump {
            self.scroll_y_px = self.layout.page_y(page_idx);
        }
        self.scroll_y_px = self.layout.clamp_scroll(self.scroll_y_px, viewport_h_px);
        self.last_layout_key = Some(key);
        self.page_cache.clear();
        self.image_proto = None;
        self.last_compose_key = None;
    }

    /// Drop cached page bitmaps that are far from the visible window.
    /// Keeps a small prefetch margin on either side so light scroll
    /// hits the cache instead of re-rendering.
    pub fn evict_far_pages(&mut self, visible: std::ops::Range<usize>) {
        const MARGIN: usize = 1;
        let lo = visible.start.saturating_sub(MARGIN);
        let hi = visible.end.saturating_add(MARGIN);
        self.page_cache.retain(|&k, _| k >= lo && k < hi);
    }

    /// Page index whose top edge is closest to the current scroll
    /// position — used for status display and as "current page" for
    /// jump operations / session save.
    pub fn current_page(&self) -> usize {
        // Pick the page that contains the viewport center; that's the
        // page the user is actually reading regardless of where the
        // top edge sits.
        let center = self.scroll_y_px + (self.viewport_px.1 as i64 / 2);
        self.layout.page_at(center).min(self.page_count.saturating_sub(1))
    }

    pub fn invalidate_compose(&mut self) {
        self.last_compose_key = None;
    }

    pub fn goto_page(&mut self, page: usize) {
        let p = page.min(self.page_count.saturating_sub(1));
        self.scroll_y_px = self
            .layout
            .clamp_scroll(self.layout.page_y(p), self.viewport_px.1);
        self.invalidate_compose();
    }

    pub fn next_page(&mut self, count: usize) {
        let target = self.current_page().saturating_add(count.max(1));
        self.goto_page(target);
    }
    pub fn prev_page(&mut self, count: usize) {
        let target = self.current_page().saturating_sub(count.max(1));
        self.goto_page(target);
    }
    pub fn first_page(&mut self) {
        self.scroll_y_px = 0;
        self.invalidate_compose();
    }
    pub fn last_page(&mut self) {
        self.scroll_y_px = self
            .layout
            .clamp_scroll(i64::MAX, self.viewport_px.1);
        self.invalidate_compose();
    }

    pub fn toggle_dark(&mut self) {
        self.dark = !self.dark;
        // dark flag is part of LayoutKey → cache will rebuild next
        // frame. Force compose invalidation to repaint immediately.
        self.invalidate_compose();
    }

    /// Multiply zoom by `factor`, clamp to a sensible range. Layout
    /// is rebuilt on the next frame because fit_width depends on it;
    /// scroll position auto-adjusts to keep the same logical reading
    /// position (see `ensure_layout`).
    pub fn zoom_by(&mut self, factor: f32) {
        let new = (self.zoom * factor).clamp(0.25, 8.0);
        if (new - self.zoom).abs() < f32::EPSILON {
            return;
        }
        self.zoom = new;
        if (self.zoom - 1.0).abs() < 0.001 {
            self.scroll_x = 0.0;
        }
        self.invalidate_compose();
    }

    pub fn zoom_reset(&mut self) {
        self.zoom = 1.0;
        self.scroll_x = 0.0;
        self.invalidate_compose();
    }

    /// Scroll vertically by a number of pixels (positive = down).
    /// Clamped to document bounds.
    pub fn scroll_by_px(&mut self, dy_px: i64) {
        let new = self
            .layout
            .clamp_scroll(self.scroll_y_px + dy_px, self.viewport_px.1);
        if new != self.scroll_y_px {
            self.scroll_y_px = new;
            self.invalidate_compose();
        }
    }

    /// Vertical scroll by a fraction of the viewport height
    /// (e.g. 0.1 = 10% of a screen).
    pub fn scroll_by_screens(&mut self, dy_screens: f32) {
        let viewport_h = self.viewport_px.1 as i64;
        self.scroll_by_px((viewport_h as f32 * dy_screens).round() as i64);
    }

    /// Horizontal scroll, only meaningful when fit_width > viewport_w
    /// (i.e. zoomed in past 100% width).
    pub fn scroll_x_by(&mut self, dx_screens: f32) {
        let (vw, _) = self.viewport_px;
        let overflow = self.layout.fit_width_px.saturating_sub(vw) as f32;
        if overflow <= 0.0 || vw == 0 {
            return;
        }
        let dx = (vw as f32 * dx_screens) / overflow;
        let new = (self.scroll_x + dx).clamp(0.0, 1.0);
        if (new - self.scroll_x).abs() > f32::EPSILON {
            self.scroll_x = new;
            self.invalidate_compose();
        }
    }

    pub fn enter_visual(&mut self) {
        // Anchor the selection to the page currently in the viewport
        // center, with a small default rectangle.
        let page = self.current_page();
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
        self.selection_page = page;
        self.mode = Mode::Visual;
        self.status = "VISUAL — hjkl/arrows move · HJKL resize · drag mouse · y save · Esc".into();
        self.invalidate_compose();
    }

    pub fn exit_visual(&mut self) {
        self.selection = None;
        self.drag_anchor = None;
        self.mode = Mode::Normal;
        self.status.clear();
        self.invalidate_compose();
    }

    /// Save the current Visual-mode selection as a highlight on the
    /// selection's bound page and return to Normal mode.
    pub fn save_selection(&mut self) {
        if let Some(sel) = self.selection.take() {
            let color = HIGHLIGHT_COLORS[self.selection_color_idx % HIGHLIGHT_COLORS.len()];
            self.highlights.add(Highlight {
                page: self.selection_page,
                x: sel.x,
                y: sel.y,
                w: sel.w,
                h: sel.h,
                color: color.hex.into(),
                note: None,
            });
            self.highlight_revision += 1;
            self.status = format!(
                "highlight saved on page {} ({})",
                self.selection_page + 1,
                color.name
            );
        }
        self.drag_anchor = None;
        self.mode = Mode::Normal;
        self.invalidate_compose();
    }

    pub fn cycle_color(&mut self) {
        self.selection_color_idx = (self.selection_color_idx + 1) % HIGHLIGHT_COLORS.len();
        let color = HIGHLIGHT_COLORS[self.selection_color_idx];
        self.status = format!("color: {}", color.name);
        self.invalidate_compose();
    }

    /// Move/resize the active Visual-mode selection in normalised
    /// page-space (0..1).
    pub fn nudge_selection(&mut self, dx: f32, dy: f32, resize: bool) {
        if let Some(sel) = self.selection.as_mut() {
            *sel = nudge_rect(*sel, dx, dy, resize);
            self.invalidate_compose();
        }
    }

    pub fn delete_last_highlight_on_current_page(&mut self) -> bool {
        let page = self.current_page();
        let pos = self
            .highlights
            .items
            .iter()
            .rposition(|h| h.page == page);
        if let Some(idx) = pos {
            self.highlights.items.remove(idx);
            self.highlight_revision += 1;
            self.invalidate_compose();
            true
        } else {
            false
        }
    }

    /// Convert a terminal (column, row) cell coordinate into
    /// `(page_idx, normalised_x_in_page, normalised_y_in_page)`.
    /// Returns `None` if the cell is outside the image area or below
    /// the last page (i.e. inside an inter-page gap or the empty
    /// area past the doc end).
    pub fn cell_to_page_coord(&self, col: u16, row: u16) -> Option<(usize, f32, f32)> {
        let area = self.image_area;
        if col < area.x || row < area.y {
            return None;
        }
        let local_col = col - area.x;
        let local_row = row - area.y;
        if local_col >= area.width || local_row >= area.height {
            return None;
        }
        let (cell_w, cell_h) = self.cell_size_px;
        let viewport_x = local_col as i64 * cell_w as i64;
        let viewport_y = local_row as i64 * cell_h as i64;

        let (vw, _) = self.viewport_px;
        let fw = self.layout.fit_width_px;
        // X offset of the rendered page strip within the viewport.
        let page_x_origin: i64 = if fw <= vw {
            ((vw - fw) / 2) as i64
        } else {
            -(((fw - vw) as f32) * self.scroll_x).round() as i64
        };
        let page_x = viewport_x - page_x_origin;
        if page_x < 0 || page_x >= fw as i64 {
            return None;
        }

        let doc_y = self.scroll_y_px + viewport_y;
        let page_idx = self.layout.page_at(doc_y);
        if page_idx >= self.page_count {
            return None;
        }
        let page_top = self.layout.page_y(page_idx);
        let page_h = self.layout.page_h(page_idx);
        if page_h == 0 {
            return None;
        }
        let local_y = doc_y - page_top;
        if local_y < 0 || local_y >= page_h as i64 {
            // Inside an inter-page gap.
            return None;
        }
        let nx = (page_x as f32 / fw as f32).clamp(0.0, 1.0);
        let ny = (local_y as f32 / page_h as f32).clamp(0.0, 1.0);
        Some((page_idx, nx, ny))
    }

    /// Begin a mouse-drag highlight at `(col, row)`. Switches into
    /// Visual mode with a zero-size selection at the click point.
    pub fn mouse_drag_start(&mut self, col: u16, row: u16) {
        if let Some((page, nx, ny)) = self.cell_to_page_coord(col, row) {
            self.drag_anchor = Some((page, nx, ny));
            self.selection_page = page;
            self.selection = Some(Rect01 { x: nx, y: ny, w: 0.0, h: 0.0 });
            self.mode = Mode::Visual;
            self.status = "Drag to select · release to save · Esc to cancel".into();
            self.invalidate_compose();
        }
    }

    /// Update the in-progress mouse-drag selection. No-op if no drag
    /// is active or the cursor moved off the page strip.
    pub fn mouse_drag_to(&mut self, col: u16, row: u16) {
        let Some((anchor_page, ax, ay)) = self.drag_anchor else {
            return;
        };
        let Some((cur_page, nx, ny)) = self.cell_to_page_coord(col, row) else {
            return;
        };
        // Confine the selection to the anchor page — cross-page
        // selection isn't representable in the highlight store, and
        // clamping to the anchor page keeps the rectangle visible.
        if cur_page != anchor_page {
            return;
        }
        let x0 = ax.min(nx);
        let y0 = ay.min(ny);
        let x1 = ax.max(nx);
        let y1 = ay.max(ny);
        self.selection = Some(Rect01 {
            x: x0,
            y: y0,
            w: (x1 - x0).max(0.001),
            h: (y1 - y0).max(0.001),
        });
        self.selection_page = anchor_page;
        self.invalidate_compose();
    }

    /// Finalise a mouse-drag selection. Saves if the rectangle is
    /// large enough to be a real highlight, otherwise just discards
    /// it (treats single click as "exit Visual mode without saving").
    pub fn mouse_drag_end(&mut self) {
        let Some(_) = self.drag_anchor.take() else {
            return;
        };
        let big_enough = self
            .selection
            .map(|s| s.w >= 0.01 && s.h >= 0.01)
            .unwrap_or(false);
        if big_enough {
            self.save_selection();
        } else {
            self.exit_visual();
        }
    }

    pub fn persist_highlights(&self) -> Result<()> {
        self.highlights.save(&self.path)
    }

    pub fn persist_session(&self) -> Result<()> {
        Session {
            page: self.current_page(),
            dark: self.dark,
        }
        .save(&self.path)
    }
}

/// Inter-page gap in pixels. Small but visible — large enough that
/// users notice page boundaries when scrolling, small enough not to
/// waste vertical space.
pub fn layout_gap_px() -> u32 {
    8
}

/// Pure helper: compute the new selection rectangle after a Visual
/// mode hjkl/HJKL keypress. Sliding leaves size fixed; resizing
/// grows/shrinks from the bottom-right corner. Both modes clamp to
/// stay inside the page and never collapse below 1% × 1%.
pub fn nudge_rect(sel: Rect01, dx: f32, dy: f32, resize: bool) -> Rect01 {
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

    #[test]
    fn nudge_rect_slides_within_bounds() {
        let sel = Rect01 { x: 0.4, y: 0.4, w: 0.2, h: 0.2 };
        let r = nudge_rect(sel, 0.1, 0.0, false);
        assert!((r.x - 0.5).abs() < 1e-4);
        assert_eq!(r.w, 0.2);
    }

    #[test]
    fn nudge_rect_clamps_when_sliding_off_edge() {
        let sel = Rect01 { x: 0.9, y: 0.0, w: 0.2, h: 0.2 };
        let r = nudge_rect(sel, 0.5, 0.0, false);
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
        let mut sel = Rect01 { x: 0.1, y: 0.1, w: 0.2, h: 0.2 };
        for _ in 0..50 {
            sel = nudge_rect(sel, -0.1, -0.1, true);
        }
        assert!((sel.w - 0.01).abs() < 1e-4);
        assert!((sel.h - 0.01).abs() < 1e-4);
    }
}
