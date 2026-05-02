use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use image::{DynamicImage, RgbaImage};
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

/// Cache key for the *per-page overlay* tier. The composited
/// (with-overlays) RgbaImage cached in `overlay_cache` is keyed on
/// this so a mouse-drag selection only rebuilds the bitmap of the
/// page the selection lives on — everything else keeps its
/// already-overlaid copy across frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageOverlayKey {
    pub layout: LayoutKey,
    pub highlight_revision: u64,
    /// `None` unless the active Visual-mode selection lives on
    /// this page. Encodes (x, y, w, h, color_idx) at 1/10000th
    /// resolution so f32 rounding doesn't trigger spurious
    /// rebuilds.
    pub sel_sig: Option<(u32, u32, u32, u32, usize)>,
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
    /// Per-page bitmap with saved highlights and (if applicable)
    /// the active selection blended in. Rebuilt on overlay change
    /// for a single page; everything else stays cached. This is
    /// what the drag-time hot path reads — without it, every
    /// mouse-move event re-cloned every visible page.
    pub overlay_cache: HashMap<usize, (RgbaImage, PageOverlayKey)>,
    pub image_proto: Option<StatefulProtocol>,
    pub last_compose_key: Option<ComposeKey>,

    pub highlights: HighlightStore,
    /// Bumped on every highlight add/delete so the compose cache
    /// invalidates without re-hashing the store.
    pub highlight_revision: u64,
    /// Set by `App::new` when the user passed a starting page (or the
    /// session restored one). Consumed by the first `ensure_layout`
    /// call to compute the initial scroll offset, then cleared.
    pub pending_initial_page: Option<usize>,
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
        // Highlights are an enhancement — a corrupt or unreadable
        // store shouldn't keep the user from opening the document.
        // Surface the error to stderr and proceed with an empty
        // store; the user can move/delete the bad file by hand.
        let highlights = HighlightStore::load(path).unwrap_or_else(|e| {
            eprintln!("warning: could not load highlights: {e:#}");
            HighlightStore::default()
        });
        // Empty layout — first `ensure_image` call builds a real one
        // once the viewport size is known.
        let layout = PageLayout::build(&[], 0, 0);
        let app = Self {
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
            overlay_cache: HashMap::new(),
            image_proto: None,
            last_compose_key: None,
            highlights,
            highlight_revision: 0,
            pending_initial_page: if page < page_count { Some(page) } else { None },
            should_quit: false,
        };
        Ok(app)
    }

    /// Rebuild the layout if `LayoutKey` changed. On the very first
    /// call (no prior layout), resolves any `pending_initial_page`
    /// into a real scroll offset. Called at the top of
    /// `ui::ensure_image`.
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

        // Across-zoom scroll preservation: capture the user's current
        // logical reading position *before* we throw away the old
        // layout. We measure as "fraction into the current page";
        // this stays sensible across resize and zoom. If the user is
        // currently parked in an inter-page gap, `page_at` assigns
        // the gap to the page above, so `local_y` can exceed
        // `cur_page_h` by up to `gap_px`. Clamp to [0,1] so the
        // restored position never overshoots into the next page.
        let preserve = if self.last_layout_key.is_some() {
            let cur_page = self.layout.page_at(self.scroll_y_px);
            let cur_page_y = self.layout.page_y(cur_page);
            let cur_page_h = self.layout.page_h(cur_page).max(1) as f32;
            let frac =
                ((self.scroll_y_px - cur_page_y) as f32 / cur_page_h).clamp(0.0, 1.0);
            Some((cur_page, frac))
        } else {
            None
        };

        self.layout = PageLayout::build(&self.page_metrics, fit_width_px, layout_gap_px());

        if let Some((cur_page, frac)) = preserve {
            let new_page_y = self.layout.page_y(cur_page);
            let new_page_h = self.layout.page_h(cur_page) as f32;
            self.scroll_y_px = new_page_y + (frac * new_page_h) as i64;
        } else if let Some(page_idx) = self.pending_initial_page.take() {
            self.scroll_y_px = self.layout.page_y(page_idx);
        }

        self.scroll_y_px = self.layout.clamp_scroll(self.scroll_y_px, viewport_h_px);
        self.last_layout_key = Some(key);
        self.page_cache.clear();
        self.overlay_cache.clear();
        self.image_proto = None;
        self.last_compose_key = None;
    }

    /// Drop cached page bitmaps (and their overlay derivatives) that
    /// are far from the visible window. Keeps a small prefetch
    /// margin on either side so light scroll hits the cache instead
    /// of re-rendering.
    pub fn evict_far_pages(&mut self, visible: std::ops::Range<usize>) {
        const MARGIN: usize = 1;
        let lo = visible.start.saturating_sub(MARGIN);
        let hi = visible.end.saturating_add(MARGIN);
        self.page_cache.retain(|&k, _| k >= lo && k < hi);
        self.overlay_cache.retain(|&k, _| k >= lo && k < hi);
    }

    /// Page that contains the viewport center — what the user is
    /// *reading*. Status line and session save use this so the
    /// number on screen matches the visual focus, not the scroll
    /// position. Use `leading_page()` for navigation.
    pub fn current_page(&self) -> usize {
        let center = self.scroll_y_px + (self.viewport_px.1 as i64 / 2);
        self.layout.page_at(center).min(self.page_count.saturating_sub(1))
    }

    /// Page that contains the *top* of the viewport. This is what
    /// `j`/`k` advance from: a `j` press should always reveal new
    /// content, never skip a page just because a short page is
    /// already partially visible above the viewport center.
    pub fn leading_page(&self) -> usize {
        self.layout
            .page_at(self.scroll_y_px)
            .min(self.page_count.saturating_sub(1))
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
        let target = self.leading_page().saturating_add(count.max(1));
        self.goto_page(target);
    }
    pub fn prev_page(&mut self, count: usize) {
        let target = self.leading_page().saturating_sub(count.max(1));
        self.goto_page(target);
    }
    pub fn first_page(&mut self) {
        self.scroll_y_px = 0;
        self.invalidate_compose();
    }
    /// `G` lands on the *top* of the last page, not the doc bottom.
    /// Matches `:N` and counted `NG` semantics so all "go to page p"
    /// paths agree on where p starts.
    pub fn last_page(&mut self) {
        let last = self.page_count.saturating_sub(1);
        self.scroll_y_px = self
            .layout
            .clamp_scroll(self.layout.page_y(last), self.viewport_px.1);
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
    /// Clamped to document bounds. Saturating add guards against an
    /// `i64::MIN`/`i64::MAX` `dy_px` from a user-supplied count.
    pub fn scroll_by_px(&mut self, dy_px: i64) {
        let new = self
            .layout
            .clamp_scroll(self.scroll_y_px.saturating_add(dy_px), self.viewport_px.1);
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
        self.pending.clear();
        self.status = "VISUAL — hjkl/arrows move, HJKL resize, drag mouse, y save, Esc".into();
        self.invalidate_compose();
    }

    pub fn exit_visual(&mut self) {
        self.selection = None;
        self.drag_anchor = None;
        self.mode = Mode::Normal;
        self.status.clear();
        self.invalidate_compose();
    }

    /// Yank the active Visual-mode selection: extract its text,
    /// copy to the clipboard, and (if `save`) persist a highlight on
    /// the selection's bound page. Returns to Normal mode.
    ///
    /// `save = true` is the `y` keybinding (highlight + copy);
    /// `save = false` is `Y` (copy text without leaving a yellow
    /// box). Either way, an empty extraction (image-only region) is
    /// surfaced in the status line rather than silently swallowed —
    /// users want to know if their quote didn't actually land.
    pub fn yank_selection(&mut self, save: bool) {
        let Some(sel) = self.selection.take() else {
            self.mode = Mode::Normal;
            return;
        };
        let color = HIGHLIGHT_COLORS[self.selection_color_idx % HIGHLIGHT_COLORS.len()];
        let metrics = self.page_metrics.get(self.selection_page).copied();

        // Try to extract text; an Err here is a real pdfium failure
        // (corrupt page), distinct from "empty rect" which returns
        // Ok("").
        let text = match metrics {
            Some(m) => crate::text::extract_rect(&self.document, self.selection_page, sel, &m)
                .unwrap_or_default(),
            None => String::new(),
        };
        let text = text.trim();

        let copy_outcome = if !text.is_empty() {
            Some(crate::clipboard::copy(text))
        } else {
            None
        };

        if save {
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
        }

        // Status message: tell the user what actually happened.
        self.status = match (save, copy_outcome) {
            (true, Some(o)) if o.truncated => {
                format!("highlight saved + copied {} bytes (truncated)", o.bytes)
            }
            (true, Some(o)) => format!("highlight saved + copied {} bytes", o.bytes),
            (true, None) => format!(
                "highlight saved on page {} (no text in selection)",
                self.selection_page + 1
            ),
            (false, Some(o)) if o.truncated => {
                format!("copied {} bytes (truncated)", o.bytes)
            }
            (false, Some(o)) => format!("copied {} bytes", o.bytes),
            (false, None) => "no text in selection".into(),
        };

        self.drag_anchor = None;
        self.mode = Mode::Normal;
        self.invalidate_compose();
    }

    /// Backwards-compatible name for `y` (save + copy). Visual-mode
    /// keybinding `y` and the search-helper module both call this.
    pub fn save_selection(&mut self) {
        self.yank_selection(true);
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
    /// area past the doc end). Thin wrapper over `cell_to_page_coord_pure`
    /// — the pure version is what unit tests exercise.
    pub fn cell_to_page_coord(&self, col: u16, row: u16) -> Option<(usize, f32, f32)> {
        cell_to_page_coord_pure(
            col,
            row,
            self.image_area,
            self.cell_size_px,
            self.viewport_px,
            self.scroll_x,
            self.scroll_y_px,
            &self.layout,
            self.page_count,
        )
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

/// Pure version of `App::cell_to_page_coord`, extracted so the
/// many branches of the mouse-coord transform can be unit-tested
/// without constructing a `PdfDocument`.
pub fn cell_to_page_coord_pure(
    col: u16,
    row: u16,
    image_area: Rect,
    cell_size_px: (u16, u16),
    viewport_px: (u32, u32),
    scroll_x: f32,
    scroll_y_px: i64,
    layout: &PageLayout,
    page_count: usize,
) -> Option<(usize, f32, f32)> {
    if col < image_area.x || row < image_area.y {
        return None;
    }
    let local_col = col - image_area.x;
    let local_row = row - image_area.y;
    if local_col >= image_area.width || local_row >= image_area.height {
        return None;
    }
    let (cell_w, cell_h) = cell_size_px;
    let viewport_x = local_col as i64 * cell_w as i64;
    let viewport_y = local_row as i64 * cell_h as i64;

    let (vw, _) = viewport_px;
    let fw = layout.fit_width_px;
    let page_x_origin: i64 = if fw <= vw {
        ((vw - fw) / 2) as i64
    } else {
        -(((fw - vw) as f32) * scroll_x).round() as i64
    };
    let page_x = viewport_x - page_x_origin;
    if page_x < 0 || page_x >= fw as i64 {
        return None;
    }

    let doc_y = scroll_y_px + viewport_y;
    let page_idx = layout.page_at(doc_y);
    if page_idx >= page_count {
        return None;
    }
    let page_top = layout.page_y(page_idx);
    let page_h = layout.page_h(page_idx);
    if page_h == 0 {
        return None;
    }
    let local_y = doc_y - page_top;
    if local_y < 0 || local_y >= page_h as i64 {
        return None;
    }
    let nx = (page_x as f32 / fw as f32).clamp(0.0, 1.0);
    let ny = (local_y as f32 / page_h as f32).clamp(0.0, 1.0);
    Some((page_idx, nx, ny))
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
    use crate::pdf::PageMetrics;

    fn metrics(n: usize) -> Vec<PageMetrics> {
        // 100×200pt portrait pages.
        vec![PageMetrics { width_pts: 100.0, height_pts: 200.0 }; n]
    }

    fn layout_for(n: usize, fit_width_px: u32) -> PageLayout {
        PageLayout::build(&metrics(n), fit_width_px, 8)
    }

    #[test]
    fn cell_to_page_coord_outside_image_area_is_none() {
        // image_area at (5,2)..(85,42); cell (4,1) is before x; cell
        // (90,3) is past width; cell (10,50) is past height.
        let layout = layout_for(3, 100);
        let area = Rect { x: 5, y: 2, width: 80, height: 40 };
        let cell = (10u16, 20u16);
        assert!(cell_to_page_coord_pure(4, 10, area, cell, (800, 800), 0.0, 0, &layout, 3).is_none());
        assert!(cell_to_page_coord_pure(10, 1, area, cell, (800, 800), 0.0, 0, &layout, 3).is_none());
        assert!(cell_to_page_coord_pure(90, 3, area, cell, (800, 800), 0.0, 0, &layout, 3).is_none());
        assert!(cell_to_page_coord_pure(10, 50, area, cell, (800, 800), 0.0, 0, &layout, 3).is_none());
    }

    #[test]
    fn cell_to_page_coord_centered_page_at_zero_zoom_one() {
        // viewport_w=100, fit_width_px=100 → no centering offset.
        // image area starts at (0,0) for clean math; cell_size 1×1 px.
        let layout = layout_for(3, 100);  // page 0 height = 200
        let area = Rect { x: 0, y: 0, width: 100, height: 200 };
        let cell_size = (1u16, 1u16);
        let viewport = (100u32, 200u32);

        // Click at (50, 100) → page 0, nx=0.5, ny=0.5.
        let r = cell_to_page_coord_pure(50, 100, area, cell_size, viewport, 0.0, 0, &layout, 3);
        let (p, nx, ny) = r.expect("center hit must land on page 0");
        assert_eq!(p, 0);
        assert!((nx - 0.5).abs() < 1e-3);
        assert!((ny - 0.5).abs() < 1e-3);
    }

    #[test]
    fn cell_to_page_coord_inside_inter_page_gap_is_none() {
        // Page 0 ends at y=200; gap [200,208); page 1 starts at 208.
        let layout = layout_for(3, 100);
        let area = Rect { x: 0, y: 0, width: 100, height: 250 };
        let r = cell_to_page_coord_pure(
            50, 204, area, (1, 1), (100, 250), 0.0, 0, &layout, 3,
        );
        assert!(r.is_none(), "click in gap should be None, got {r:?}");
    }

    #[test]
    fn cell_to_page_coord_past_end_is_none() {
        // 3 pages, scrolled past total — click at top of viewport
        // is past doc end.
        let layout = layout_for(3, 100);
        let area = Rect { x: 0, y: 0, width: 100, height: 50 };
        // Total = 200*3 + 8*2 = 616. Scroll well past it.
        let r = cell_to_page_coord_pure(
            50, 0, area, (1, 1), (100, 50), 0.0, 9999, &layout, 3,
        );
        assert!(r.is_none(), "past-end click should be None");
    }

    #[test]
    fn cell_to_page_coord_horizontal_centering_when_zoomed_out() {
        // fit_width = 50, viewport_w = 100 → page strip centered with
        // 25px of bg on each side. A click at viewport_x=10 sits in
        // the left bg → None. Click at viewport_x=50 (center) → nx=0.5.
        let layout = layout_for(3, 50);
        let area = Rect { x: 0, y: 0, width: 100, height: 100 };
        let viewport = (100u32, 100u32);
        let cell = (1u16, 1u16);

        let r_bg = cell_to_page_coord_pure(10, 10, area, cell, viewport, 0.0, 0, &layout, 3);
        assert!(r_bg.is_none(), "click in centering bg must be None");

        let r = cell_to_page_coord_pure(50, 10, area, cell, viewport, 0.0, 0, &layout, 3);
        let (_, nx, _) = r.expect("center click must hit page 0");
        assert!((nx - 0.5).abs() < 1e-2, "nx was {nx}");
    }

    #[test]
    fn cell_to_page_coord_horizontal_scroll_when_zoomed_in() {
        // fit_width=200, viewport_w=100 → 100px of horizontal
        // overflow. scroll_x=0.5 → page strip starts at viewport_x=-50.
        // Click at viewport_x=50 → page_x = 50 - (-50) = 100; fw=200
        // → nx = 0.5.
        let layout = layout_for(3, 200);
        let area = Rect { x: 0, y: 0, width: 100, height: 200 };
        let viewport = (100u32, 200u32);
        let cell = (1u16, 1u16);

        let r = cell_to_page_coord_pure(50, 100, area, cell, viewport, 0.5, 0, &layout, 3);
        let (_, nx, _) = r.expect("center click must hit page 0");
        assert!((nx - 0.5).abs() < 1e-2, "nx was {nx}");
    }

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
