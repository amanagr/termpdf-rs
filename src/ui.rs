//! ratatui frame composition: continuous-scroll page strip + 1-row
//! status line + ? help overlay. The image itself is rendered by
//! ratatui-image, which handles Kitty/Sixel chunking transparently.
//!
//! Render pipeline (called per frame, but each layer is cached):
//!   1. **layout** — once `LayoutKey {fit_width, dark}` is known,
//!      build per-page pixel heights and y-offsets via
//!      `layout::PageLayout::build`. Cheap: PDF metadata only.
//!   2. **per-page render** — for each page intersecting the
//!      viewport, lazily rasterise via `pdf::render_page_at_width`
//!      and stash in `app.page_cache`. Pages outside a small
//!      sliding window are evicted so memory stays bounded.
//!   3. **compose** — paint a viewport-sized canvas: blit each
//!      visible page (clipped to the viewport rect), alpha-blend
//!      the saved highlights for that page, then alpha-blend the
//!      Visual-mode selection if its bound page is among them.
//!   4. **submit** — wrap the canvas in a `StatefulProtocol` and
//!      hand it to ratatui-image for kitty-graphics encoding.

use anyhow::Result;
use image::{DynamicImage, Rgba, RgbaImage};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::StatefulImage;

use crate::app::{App, ComposeKey, HighlightsBakedKey, LayoutKey, Mode, PageOverlayKey};
use crate::compose::{fill_rect_blend, fill_rect_rgba, norm_to_pixels, outline_rect};
use crate::dark;
use crate::highlight::{rgb_from_hex, Rect01, HIGHLIGHT_COLORS};
use crate::pdf;

/// Cap on `fit_width_px`. At extreme zoom on a 4K terminal
/// `viewport_w * zoom` runs into the tens of thousands; pdfium
/// happily produces gigantic pixmaps that stall every render. We
/// cap the layout width so the bitmap and the layout always agree;
/// the user just stops gaining sharper pixels beyond the cap (which
/// is well past the threshold where you'd be reading a single
/// character per viewport anyway).
pub const MAX_FIT_WIDTH_PX: u32 = 4096;

/// Cap on cold-page renders per kitty draw. Each cold render emits a
/// ~250–500 KB base64 PNG transmit through tmux passthrough; a big
/// jump like `100G` on a 600-page book can put 3+ cold pages in the
/// visible region simultaneously, and shipping their transmits in one
/// frame pushed Ghostty's renderer over its limit (window-vanish,
/// observed 2026-05-03). One cold render per draw + a forced
/// next-frame redraw spreads the burst into ~3 frames at 60 Hz.
pub const MAX_COLD_RENDERS_PER_DRAW: usize = 1;

/// Cap on transmit emissions per kitty draw. A revision flip
/// (highlight added, selection moved, search advanced) marks every
/// visible cached page stale at the same time — left unbounded, a
/// 5-page-visible viewport ships ~1.5 MB of base64 in one frame.
/// The budget keeps each frame's transmit bytes bounded; the
/// `pending_cold_redraw` mechanism in run_loop catches the deferred
/// pages up on subsequent frames at ~50 ms intervals. Pages with no
/// prior transmit can't be deferred (placement without a prior
/// transmit shows garbled cells), so the cold-render budget at the
/// render phase is the upstream guard for those.
pub const MAX_TRANSMITS_PER_DRAW: usize = 2;

/// What to do with a single page on a kitty draw, given the cache
/// hit/miss and the current rapid-scroll + budget state. Pulled out
/// as a pure function so the staggering logic is unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColdRenderDecision {
    /// Page is already in `page_cache`; just touch the LRU.
    AlreadyCached,
    /// Page is cold but the budget allows a render this frame.
    Render,
    /// Page is cold and we're skipping it because the per-frame cold
    /// budget is exhausted (multiple cold pages this frame). The
    /// caller should set `pending_cold_redraw` so the run-loop forces
    /// another draw and we catch up next frame at +16 ms.
    DeferBudget,
    /// Page is cold and we're skipping it because a rapid-scroll burst
    /// is in progress. The caller MUST NOT set `pending_cold_redraw`
    /// — that would force an immediate redraw, which would re-defer
    /// (rapid is still true), which would set the flag again, looping
    /// at 60 Hz until the burst ends. Instead, the run-loop's settle
    /// timer fires a single catch-up draw once input goes idle.
    DeferRapid,
}

pub(crate) fn plan_cold_render(
    is_cached: bool,
    rapid: bool,
    cold_budget_remaining: usize,
) -> ColdRenderDecision {
    if is_cached {
        return ColdRenderDecision::AlreadyCached;
    }
    if rapid {
        return ColdRenderDecision::DeferRapid;
    }
    if cold_budget_remaining == 0 {
        return ColdRenderDecision::DeferBudget;
    }
    ColdRenderDecision::Render
}

/// Plan which transmits to defer this frame given a per-frame budget.
/// Each input is `(need_transmit, page_idx, has_prior_transmit_on_terminal)`.
/// Returns the indices into the input slice whose `need_transmit` should
/// be flipped to false. Pages without a prior transmit are never
/// deferred — placement without bytes shows garbled cells. Among
/// deferrable transmits, those farthest from `current_page` are shed
/// first so the active page always re-paints same-frame.
pub(crate) fn plan_transmit_deferrals(
    blits: &[(bool, usize, bool)],
    current_page: usize,
    budget: usize,
) -> Vec<usize> {
    let mut candidates: Vec<usize> = blits
        .iter()
        .enumerate()
        .filter_map(
            |(i, (need, _, has_prior))| {
                if *need && *has_prior {
                    Some(i)
                } else {
                    None
                }
            },
        )
        .collect();
    let needed = blits.iter().filter(|(n, _, _)| *n).count();
    if needed <= budget {
        return Vec::new();
    }
    // Keep up to `budget` transmits; among deferrable candidates the
    // CLOSEST to current_page survive — distance ascending = drop the
    // farthest. Stable secondary sort by index for determinism.
    candidates.sort_by_key(|&i| {
        let p = blits[i].1;
        let dist = p.abs_diff(current_page);
        (dist, i)
    });
    // How many deferrable transmits can we shed? Non-deferrable
    // (no-prior) transmits stay in the count regardless.
    let non_deferrable = needed - candidates.len();
    let max_keep = budget.saturating_sub(non_deferrable);
    if candidates.len() <= max_keep {
        return Vec::new();
    }
    candidates.split_off(max_keep)
}

/// Soft byte budget on `App::page_cache`. A 4-byte-per-pixel RGBA
/// budget; 256 MB ≈ 64 megapixels of cached pages, which is several
/// dozen typical pages or a smaller number of big scanned ones.
/// Override at startup with `$TERMPDF_CACHE_MB`.
///
/// Cached: this fires on every frame from the budget enforcement path;
/// without the OnceLock it was paying an env-scan syscall per draw.
pub fn page_cache_budget_bytes() -> usize {
    use std::sync::OnceLock;
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let mb = std::env::var("TERMPDF_CACHE_MB")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(256);
        mb.saturating_mul(1024 * 1024)
    })
}

pub fn draw(f: &mut Frame, app: &mut App<'_>) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    let img_area = chunks[0];
    let status_area = chunks[1];

    // Stash the image area + cell size so dispatch_mouse can map
    // terminal cells back to page coordinates.
    app.image_area = img_area;
    app.cell_size_px = app.picker.font_size();

    // Suppress the kitty image entirely while help/TOC popups are
    // open. The kitty graphics protocol re-flows around any cell we
    // paint over (the popup `Clear`), and on some terminals that
    // breaks the unicode-placeholder mapping for the cells OUTSIDE
    // the popup too — the user saw the right-of-popup region go
    // blank. With the image suppressed, the popup paints onto a
    // clean default background and reading resumes correctly when
    // the popup closes (the next ensure_image rebuilds the protocol).
    let popup_open = app.show_help || app.show_toc;
    if popup_open {
        // Drop the cached protocol so the next paint after the popup
        // closes uploads a fresh image.
        app.image_proto = None;
        app.last_compose_key = None;
        // Fill the image area with a soft background so the popup's
        // surroundings aren't stark black.
        f.render_widget(
            Block::default().style(Style::default().bg(Color::Black)),
            img_area,
        );
    } else if app.kitty_pages.is_some() {
        // Per-page kitty placements bypass ratatui-image: each page
        // becomes its own kitty image, transmitted once. Steady scroll
        // is then just a few hundred bytes of placeholder cells per
        // frame instead of a multi-MB canvas re-encode.
        //
        // Background fill first: pages may not span the full image
        // area (centered layouts, narrower-than-viewport pages,
        // inter-page gaps). The canvas path used to fill the gap
        // pixels with `bg` during compose; here we paint a Block
        // across img_area so any cell the placement loop doesn't
        // touch falls back to the right page-background color.
        let bg = if app.dark {
            Color::Rgb(20, 20, 20)
        } else {
            Color::Rgb(240, 240, 240)
        };
        f.render_widget(Block::default().style(Style::default().bg(bg)), img_area);
        if let Err(e) = draw_pages_kitty(f, app, img_area) {
            f.render_widget(
                Paragraph::new(format!("render error: {e:#}"))
                    .style(Style::default().fg(Color::Red)),
                img_area,
            );
        }
    } else if let Err(e) = ensure_image(app, img_area) {
        f.render_widget(
            Paragraph::new(format!("render error: {e:#}")).style(Style::default().fg(Color::Red)),
            img_area,
        );
    } else if let Some(proto) = app.image_proto.as_mut() {
        // Non-kitty fallback (sixel / iterm2 / halfblocks): the
        // composed image is exactly viewport-sized (we already
        // centered/cropped while painting), so render it across the
        // full image area.
        f.render_stateful_widget(StatefulImage::<StatefulProtocol>::new(), img_area, proto);
    }

    // The active Visual-mode selection is now baked into the page
    // bitmap by `ensure_overlay`, so it travels through the kitty
    // re-upload path that already works for saved highlights.
    // The cell-overlay we used to paint here was unreliable in
    // tmux+Ghostty: ratatui-image packs each row's kitty escape into
    // column 0 and our post-image cell writes never reached the wire.

    f.render_widget(status_line(app), status_area);

    // Confine popups to the image area so their bottom border doesn't
    // overpaint the 1-row status line below it.
    if app.show_toc {
        draw_toc(f, app, img_area);
    }

    if app.show_help {
        draw_help(f, img_area);
    }
}

// The selection-overlay cell-styling helpers used to live here.
// They were dropped along with the cell-overlay path; the live
// selection now uses `fill_rect_blend` directly into the page
// bitmap (see `ensure_overlay`).

fn ensure_image(app: &mut App<'_>, area: Rect) -> Result<()> {
    // Translate the terminal area into pixels and decide on the
    // current `fit_width_px` (= viewport_w * zoom). Then make the
    // layout match.
    let (cell_w, cell_h) = app.picker.font_size();
    let viewport_w = (area.width as u32) * (cell_w as u32);
    let viewport_h = (area.height as u32) * (cell_h as u32);
    app.viewport_px = (viewport_w, viewport_h);

    let fit_width_px = (((viewport_w as f32) * app.zoom).max(1.0) as u32).min(MAX_FIT_WIDTH_PX);
    app.ensure_layout(fit_width_px, viewport_h);

    let layout_key = LayoutKey {
        fit_width_px,
        dark: app.dark,
    };

    // Ensure a bitmap exists for every visible page. Each render is
    // cached under its page index; the bitmap matches the current
    // Drain any completed worker renders into the page cache before
    // we decide what's still missing. This is what makes prefetched
    // pages "appear" in the cache between frames.
    drain_worker_results(app);

    // LayoutKey because `ensure_layout` clears the cache on change.
    let visible = app.layout.visible_pages(app.scroll_y_px, viewport_h);
    {
        let _s = crate::profile::span(crate::profile::Phase::EnsureRendered);
        for page_idx in visible.clone() {
            ensure_page_rendered(app, page_idx, fit_width_px, /*allow_failure=*/ true)?;
            app.touch_page(page_idx);
        }
    }

    // Speculatively render a few pages outside the viewport in the
    // user's scroll direction. Sent to the background worker if one's
    // available; otherwise falls through to a sync render. The render
    // worker is currently a stub (see render_worker.rs), so each
    // prefetch is a synchronous ~20 ms pdfium call here.
    //
    // During a rapid-scroll burst, skip prefetch entirely. Otherwise
    // a held-`j` pays ~40 ms of pdfium on every frame for pages the
    // user is about to scroll past anyway. The settle redraw fired
    // when input goes idle warms whatever's actually visible.
    if !app.is_rapid_scrolling() {
        let prefetch = app.prefetch_targets(visible.clone());
        for page_idx in prefetch {
            request_prefetch(app, page_idx, fit_width_px);
        }
    }

    app.evict_far_pages(visible.clone());
    app.enforce_byte_budget(page_cache_budget_bytes(), visible.clone());

    // Compose key: changes to scroll, highlight count, or viewport
    // invalidate the cached canvas. The active Visual-mode selection
    // is intentionally absent — it's drawn as a separate cell overlay
    // (see `draw_selection_overlay`), so nudging the selection doesn't
    // touch the kitty image at all.
    let compose_key = ComposeKey {
        layout: layout_key,
        viewport_w,
        viewport_h,
        scroll_y_px: app.scroll_y_px,
        scroll_x_milli: (app.scroll_x * 10000.0) as u32,
        highlight_revision: app.highlight_revision,
        selection_sig: app.selection_signature_global(),
    };
    if app.last_compose_key == Some(compose_key) && app.image_proto.is_some() {
        return Ok(());
    }

    // Make sure every visible page has a fresh overlay bitmap before
    // we stitch them together. Done in a separate loop so the borrow
    // of `app` stays mutable here, then immutable in compose.
    {
        let _s = crate::profile::span(crate::profile::Phase::EnsureOverlay);
        for page_idx in visible {
            ensure_overlay(app, page_idx, layout_key);
        }
    }

    // Two fast paths before the full compose:
    //   - Scroll-shift: only scroll_y_px changed → memmove rows, repaint strip.
    //   - Selection-only: only selection_sig changed → re-blit just the
    //     selection's page over the previous canvas.
    let _compose_span = crate::profile::span(crate::profile::Phase::Compose);
    let canvas = if let Some(c) = try_scroll_shift_canvas(app, &compose_key, viewport_w, viewport_h)
    {
        c
    } else if let Some(c) = try_selection_only_repaint(app, &compose_key, viewport_w, viewport_h) {
        c
    } else {
        compose_into_buffer(app, viewport_w, viewport_h)
    };
    drop(_compose_span);

    // Hash-equal skip: if the just-composed canvas matches the
    // previously-encoded one byte-for-byte, no need to rebuild
    // StatefulProtocol — the kitty re-upload would re-transmit
    // identical bytes. Common when the user moves the selection
    // to/from offscreen, mashes a key past a boundary, or scrolls
    // by 0 pixels (no-op). Saves ~3× the canvas size in CPU
    // (ImageSource::new hashes 8 MB; transmit_virtual base64s another
    // 8 MB; plus the wire bytes themselves).
    let h = fnv1a_hash(canvas.as_raw());
    if h != app.last_canvas_hash || app.image_proto.is_none() {
        let _s = crate::profile::span(crate::profile::Phase::BuildProtocol);
        app.image_proto = Some(app.build_protocol(DynamicImage::ImageRgba8(canvas.clone())));
        app.last_canvas_hash = h;
    }
    app.canvas_buf = Some(canvas);
    app.last_compose_key = Some(compose_key);
    app.last_selection_range = pages_touched_by_selection(app);
    Ok(())
}

/// Render path for the kitty protocol that bypasses ratatui-image.
/// Each visible page is transmitted to the terminal once with its
/// own image ID; subsequent frames emit only unicode-placeholder
/// cells. Steady-state scrolling drops from ~150 ms (full canvas
/// re-encode + pty write) to a few hundred bytes per visible page.
///
/// Trade-off vs. the canvas path: cell-quantized scrolling. Sub-cell
/// pixel offsets are dropped; in the typical reading workflow (page
/// jumps, line scroll) this is invisible. See `kitty_pages` module
/// docs for the protocol details.
fn draw_pages_kitty(f: &mut Frame, app: &mut App<'_>, area: Rect) -> Result<()> {
    let (cell_w_u8, cell_h_u8) = app.picker.font_size();
    let cell_w = cell_w_u8 as u32;
    let cell_h = cell_h_u8 as u32;
    let viewport_w = (area.width as u32) * cell_w;
    let viewport_h = (area.height as u32) * cell_h;
    app.viewport_px = (viewport_w, viewport_h);

    let fit_width_px = (((viewport_w as f32) * app.zoom).max(1.0) as u32).min(MAX_FIT_WIDTH_PX);
    app.ensure_layout(fit_width_px, viewport_h);
    let layout_key = LayoutKey {
        fit_width_px,
        dark: app.dark,
    };

    let visible_range = app.layout.visible_pages(app.scroll_y_px, viewport_h);
    // Filter out pages whose visible region rounds to 0 cells. The
    // layout treats a page with even 1 pixel intersecting the
    // viewport as "visible", but after cell-quantization the
    // placement loop would discard it — we'd pay pdfium + overlay
    // + transmit cost for a page the user can't even see. Common at
    // the inter-page gap when the user lands a scroll between two
    // page boundaries.
    let visible: Vec<usize> = visible_range
        .clone()
        .filter(|&pi| {
            visible_cell_height(
                &app.layout,
                app.scroll_y_px,
                area.height,
                pi,
                app.picker.font_size().1 as u32,
            ) > 0
        })
        .collect();
    if visible.is_empty() {
        return Ok(());
    }

    // During a rapid-scroll burst, skip pdfium render for cold pages
    // entirely. Otherwise we pay ~20 ms per cold page in pdfium even
    // though the placement loop will discard them. The settle redraw
    // after the burst will render whatever's then visible.
    //
    // Outside rapid-scroll, cap cold renders to one per draw. A big
    // jump (`100G` on a 600-page book) makes 3+ pages cold all at
    // once, and dumping their PNG transmits back-to-back in one
    // frame is ~1.5 MB of base64 through tmux passthrough — enough
    // to crash Ghostty's renderer (window-vanish, observed
    // 2026-05-03). One per draw + force-redraw next iteration shows
    // the catch-up pages popping in over ~3 frames (~50 ms) instead
    // of all at once, which Ghostty handles fine.
    let rapid = app.is_rapid_scrolling();
    let mut cold_budget = MAX_COLD_RENDERS_PER_DRAW;
    // Only budget-based deferrals trigger a `pending_cold_redraw`:
    // a budget overflow is resolved by the next frame at +16 ms,
    // but a rapid-scroll deferral can only resolve when the burst
    // ends (input goes idle). Force-redrawing while still rapid
    // would loop at 60 Hz with every draw deferring → flag set →
    // redraw → defer → ... pegging the CPU until input stops.
    // Settle redraw (run-loop) handles the rapid case instead.
    let mut deferred_for_budget = false;
    {
        let _s = crate::profile::span(crate::profile::Phase::EnsureRendered);
        for &pi in &visible {
            let is_cached = app.page_cache.contains_key(&pi);
            match plan_cold_render(is_cached, rapid, cold_budget) {
                ColdRenderDecision::AlreadyCached => {}
                ColdRenderDecision::Render => cold_budget -= 1,
                ColdRenderDecision::DeferBudget => {
                    deferred_for_budget = true;
                    continue;
                }
                ColdRenderDecision::DeferRapid => continue,
            }
            ensure_page_rendered(app, pi, fit_width_px, /*allow_failure=*/ true)?;
            app.touch_page(pi);
        }
    }
    app.pending_cold_redraw = deferred_for_budget;

    // Materialize visible_range once so the kitty registry's
    // evict_to_budget at the end of the frame can pin LAYOUT-visible
    // pages, not just the cell-filtered `visible` Vec. A page whose
    // cell-quantized height rounds to 0 (sub-cell scroll position at
    // a page boundary, page_h not divisible by cell_h) falls out of
    // `visible` but is still layout-visible and still has placeholder
    // cells in ratatui's buffer from prior frames. If it falls out of
    // pinning, the LRU evicts its image_id and queues a `_Ga=d` to
    // Ghostty; the placeholder cells suddenly reference a freed image
    // and the page renders blank until something forces a re-transmit
    // (revision flip, layout change, click).
    let visible_range_pin: Vec<usize> = visible_range.clone().collect();
    app.evict_far_pages(visible_range.clone());
    app.enforce_byte_budget(page_cache_budget_bytes(), visible_range);

    {
        let _s = crate::profile::span(crate::profile::Phase::EnsureOverlay);
        for &pi in &visible {
            // ensure_overlay early-returns when page_cache is missing,
            // so the rapid-scroll skip above propagates here for free.
            ensure_overlay(app, pi, layout_key);
        }
    }

    // Compose-phase span: cover the planning + per-page geometry math.
    let _compose = crate::profile::span(crate::profile::Phase::Compose);

    // Plan placements + decide which pages need a transmit. Pulled out
    // into a Vec so the read-only borrows on overlay_cache and
    // kitty_pages can be released before we mutate the registry +
    // ratatui buffer in the second pass below.
    struct PageBlit {
        page_idx: usize,
        image_id: u32,
        pixel_w: u32,
        pixel_h: u32,
        revision: u64,
        need_transmit: bool,
        dst_top_cell: u16,
        dst_left_cell: u16,
        height_cells: u16,
        src_top_cell: u16,
        src_left_cell: u16,
        width_cells: u16,
        /// Selection signature for this page. Non-zero when there's an
        /// active selection touching the page; `0` means no overlay
        /// should be drawn (and any prior overlay should be dropped).
        sel_sig: u64,
        /// False when the rapid-burst defer chose to skip this page's
        /// kitty placement. The blit still carries its original geometry
        /// so we can paint a "loading…" indicator over the cleared area.
        placement_active: bool,
    }

    let scroll_y = app.scroll_y_px;
    let scroll_x = app.scroll_x;
    let area_width_cells = area.width;
    let area_height_cells = area.height;

    let mut blits: Vec<PageBlit> = Vec::with_capacity(visible.len());
    for &page_idx in &visible {
        // Read dims from the highlights-baked tier when present, else
        // fall back to page_cache. Pages with no highlights / search
        // hits don't get a baked entry (saves the ~6 MB clone) and
        // the page_cache image is selection-free already, so it's a
        // suitable transmit source on its own.
        let pixel_dims = if let Some((bm, _)) = app.highlights_baked_cache.get(&page_idx) {
            Some((bm.width(), bm.height()))
        } else {
            app.page_cache
                .get(&page_idx)
                .map(|d| (d.width(), d.height()))
        };
        let Some((pixel_w, pixel_h)) = pixel_dims else {
            continue;
        };

        let page_doc_y = app.layout.page_y(page_idx);
        let page_h_px = app.layout.page_h(page_idx);
        if page_h_px == 0 {
            continue;
        }

        // Cell-quantize the scroll offset for placement purposes.
        let src_top_px = (scroll_y - page_doc_y).max(0) as u32;
        let src_top_cell = (src_top_px / cell_h.max(1)) as u16;
        let dst_top_px = (page_doc_y - scroll_y).max(0) as u32;
        let dst_top_cell = (dst_top_px / cell_h.max(1)) as u16;

        let img_h_cells = (pixel_h / cell_h.max(1)) as u16;
        let img_w_cells = (pixel_w / cell_w.max(1)) as u16;

        let max_dst_rows = area_height_cells.saturating_sub(dst_top_cell);
        let max_src_rows = img_h_cells.saturating_sub(src_top_cell);
        let height_cells = max_dst_rows.min(max_src_rows);
        if height_cells == 0 {
            continue;
        }

        // Horizontal placement. Two cases:
        //   - Image fits in viewport → center it (dst_left_cell offsets
        //     the placement area; src_left_cell stays 0).
        //   - Image overflows → no centering; src_left_cell shifts the
        //     visible window of the image based on app.scroll_x. This
        //     is what makes Left/Right arrows actually scroll the
        //     zoomed-in page horizontally on the kitty path.
        let (dst_left_cell, src_left_cell, width_cells) = if img_w_cells <= area_width_cells {
            let dl = (area_width_cells - img_w_cells) / 2;
            (dl, 0u16, img_w_cells)
        } else {
            let overflow_cells = img_w_cells - area_width_cells;
            // scroll_x is 0..=1 over the overflow span; cell-quantize.
            let src_left = ((overflow_cells as f32) * scroll_x.clamp(0.0, 1.0)).round() as u16;
            (0u16, src_left, area_width_cells)
        };
        let width_cells = width_cells.min(area_width_cells.saturating_sub(dst_left_cell));
        if width_cells == 0 {
            continue;
        }

        let revision = compute_page_revision(app, page_idx);
        let kp = app
            .kitty_pages
            .as_ref()
            .expect("kitty_pages should be Some on this draw path");
        let need_transmit = !kp.is_fresh(page_idx, layout_key, revision, pixel_w, pixel_h);
        let image_id = kp.image_id(page_idx);
        let sel_sig = app.selection_signature_for_page(page_idx);

        blits.push(PageBlit {
            page_idx,
            image_id,
            pixel_w,
            pixel_h,
            revision,
            need_transmit,
            dst_top_cell,
            dst_left_cell,
            height_cells,
            src_top_cell,
            src_left_cell,
            width_cells,
            sel_sig,
            placement_active: true,
        });
    }

    // During a rapid input burst (held j/k, mouse wheel spam) defer
    // any cold-page transmit. Each cold transmit ships ~5 MB of
    // base64; doing 5 of them per frame is what tanked draw time
    // from 58 ms to 184 ms in the user's hold-j stress test. The
    // event loop forces a catch-up draw once input goes idle (see
    // SETTLE_MS in main.rs::run_loop) so the deferred pages render
    // the moment the user lets up.
    if app.is_rapid_scrolling() {
        for b in blits.iter_mut() {
            if !b.need_transmit {
                continue;
            }
            // Defer ONLY genuinely cold pages — those whose pdfium
            // bitmap isn't in our cache yet. Pages already cached
            // (just need a re-encode because the user moved a
            // selection or edited a highlight) are cheap to transmit;
            // skipping them would make Visual-mode selection appear
            // stuck during fast `l`/`h` motion.
            //
            // Mark `placement_active = false` rather than zeroing
            // `height_cells`: the post-place pass needs the original
            // geometry to draw a "loading…" indicator centered in the
            // page's strip so the user knows the area is intentionally
            // pending instead of a glitch.
            let is_cold = !app.page_cache.contains_key(&b.page_idx);
            if is_cold {
                b.need_transmit = false;
                b.placement_active = false;
            }
        }
    }

    // Per-frame transmit budget. A revision flip (highlight added,
    // selection moved, search advanced) marks every visible cached
    // page stale at once; left unbounded the burst is the same shape
    // as the cold-render burst that crashed Ghostty (2026-05-03).
    // Defer the farthest-from-current pages whose terminal already
    // has a prior version cached; those pages place the older image
    // for one extra frame and the run-loop's force-redraw catches
    // them up next iteration.
    {
        let kp_ref = app.kitty_pages.as_ref().unwrap();
        let triples: Vec<(bool, usize, bool)> = blits
            .iter()
            .map(|b| {
                (
                    b.need_transmit,
                    b.page_idx,
                    kp_ref.has_prior_transmit(b.page_idx),
                )
            })
            .collect();
        let current_page = app.current_page();
        let to_defer = plan_transmit_deferrals(&triples, current_page, MAX_TRANSMITS_PER_DRAW);
        if !to_defer.is_empty() {
            for i in to_defer {
                blits[i].need_transmit = false;
            }
            // Trigger a follow-up frame so the deferred pages catch
            // up; same mechanism the cold-render staggering uses.
            app.pending_cold_redraw = true;
        }
    }

    // Build transmit strings for pages that need them. Three-tier
    // bitmap source:
    //   1. overlay_cache  — highlights + selection band baked in
    //      (used when this page has an active selection touching it)
    //   2. baked_cache    — highlights + search hits baked, no selection
    //   3. page_cache     — raw pdfium bitmap, no overlays
    // The selection-bake tier replaced the prior classical-placement
    // overlay path that bled across tmux panes; see ensure_overlay.
    let overlay_cache = &app.overlay_cache;
    let baked_cache = &app.highlights_baked_cache;
    let page_cache = &app.page_cache;
    let kp = app.kitty_pages.as_mut().unwrap();
    let mut transmits: Vec<Option<String>> = blits
        .iter()
        .map(|b| {
            if !b.need_transmit {
                return None;
            }
            let bm: &RgbaImage = if let Some((bm, _)) = overlay_cache.get(&b.page_idx) {
                bm
            } else if let Some((bm, _)) = baked_cache.get(&b.page_idx) {
                bm
            } else {
                page_cache.get(&b.page_idx)?.as_rgba8()?
            };
            Some(kp.build_transmit(bm, b.page_idx, layout_key, b.revision))
        })
        .collect();

    // Selection bake-into-page replaces the prior classical-placement
    // overlay. The page bitmap now carries the band (via overlay_cache;
    // ensure_overlay populates it). Drop any overlay images still
    // resident in the terminal from before this code change so the
    // terminal can free their bitmaps. The delete escapes ride out on
    // the next page transmit via take_pending_deletes below.
    let mut overlay_payloads: Vec<Option<String>> = vec![None; blits.len()];
    let overlay_marks: Vec<Option<(u64, u64, u32, u32)>> = vec![None; blits.len()];
    {
        let kp = app.kitty_pages.as_mut().unwrap();
        for b in blits.iter() {
            if kp.overlay_is_present(b.page_idx) {
                kp.overlay_drop(b.page_idx);
            }
        }
    }

    // Drain any pending kitty `a=d,d=I,i=ID` deletes from prior
    // evictions (page or overlay) and ride them in on the first
    // transmit of this frame. Doing it here rather than as a separate
    // write keeps the deletes inside the `term.draw` window and avoids
    // a second pty round-trip.
    let kp = app.kitty_pages.as_mut().unwrap();
    if let Some(mut deletes) = kp.take_pending_deletes() {
        // Prefer riding deletes on a slot that already has bytes
        // (page transmit OR overlay payload); only reject onto the
        // pending queue if NEITHER exists this frame. Move-merge the
        // existing slot string into our `deletes` buffer to avoid an
        // extra clone per merge.
        let attached = if let Some(i) = transmits.iter().position(|t| t.is_some()) {
            let existing = transmits[i].take().unwrap();
            deletes.reserve(existing.len());
            deletes.push_str(&existing);
            transmits[i] = Some(deletes);
            true
        } else if let Some(i) = overlay_payloads.iter().position(|p| p.is_some()) {
            let existing = overlay_payloads[i].take().unwrap();
            deletes.reserve(existing.len());
            deletes.push_str(&existing);
            overlay_payloads[i] = Some(deletes);
            true
        } else {
            // No transmits or overlay APCs this frame — stash the
            // deletes for next. Cheap to write back; eviction is rare
            // relative to draws.
            kp.put_back_pending_deletes(deletes);
            false
        };
        let _ = attached;
    }

    // Concatenate page transmit + overlay payload into the per-blit
    // prefix actually passed to `place_page`. The order matters:
    //   1. page transmit ships the page bitmap (no placement).
    //   2. overlay transmit ships the selection-band bitmap.
    //   3. \x1b[s + overlay classical place + \x1b[u places the
    //      overlay at the cursor (= page first cell), keeping cursor
    //      bracketed for the page's placement loop that follows.
    // Move-merge transmits + overlays into a single per-blit prefix
    // by consuming both Vecs. The (Some(t), None) branch — the steady
    // scroll case — used to clone the full ~270 KB transmit string for
    // nothing; `into_iter()` lets us hand the existing String along
    // unchanged, saving ~7-8 MB of allocation per `scroll_steady_30`.
    let combined_prefixes: Vec<Option<String>> = transmits
        .into_iter()
        .zip(overlay_payloads)
        .map(|(t, ov)| match (t, ov) {
            (None, None) => None,
            (Some(t), None) => Some(t),
            (None, Some(o)) => Some(o),
            (Some(mut t), Some(o)) => {
                t.reserve(o.len());
                t.push_str(&o);
                Some(t)
            }
        })
        .collect();

    drop(_compose);

    // Emit placements (and prefix the first cell with the transmit
    // string for any page that needed one). The Draw span on the
    // outside of `term.draw` covers the actual write to the pty.
    let buf = f.buffer_mut();
    let img_area_left = area.left();
    let img_area_top = area.top();

    // Blank the entire image area before any placement. Without this,
    // cells from the prior frame that still hold a kitty placeholder
    // (encoding an `image_id` in the fg color) survive into this
    // frame whenever no blit covers them — happens any time a visible
    // page has no cached bitmap (cold defer, budget defer, rapid
    // defer, or a page whose layout slot widened between frames).
    // Ghostty re-renders those stale cells, can't find the freed
    // image_id, and logs `warning(renderer_image): missing image for
    // virtual placement` once per cell per render-frame. At 10–20k
    // warnings/sec journald floods and the Ghostty client crashes
    // (660k entries observed in 7 days, 2026-05-04). Cell::reset() is
    // ~50 ns so even a 200×50 area costs <1 ms; cells immediately
    // overwritten by place_page below never reach the wire because
    // ratatui's diff compares the buffer's *final* state to the prior
    // frame's, not the intermediate writes.
    let kp = app.kitty_pages.as_mut().unwrap();
    let scratch = kp.place_scratch_mut();
    crate::kitty_pages::clear_page_area(buf, area, scratch);
    for (b, t) in blits.iter().zip(combined_prefixes.iter()) {
        if !b.placement_active {
            continue;
        }
        let placement_area = Rect {
            x: img_area_left.saturating_add(b.dst_left_cell),
            y: img_area_top,
            width: b.width_cells,
            height: area_height_cells,
        };
        crate::kitty_pages::place_page(
            buf,
            placement_area,
            b.page_idx,
            b.image_id,
            b.pixel_w,
            b.pixel_h,
            cell_w,
            cell_h,
            b.dst_top_cell,
            b.height_cells,
            b.src_top_cell,
            b.src_left_cell,
            b.width_cells,
            t.as_deref(),
            scratch,
        );
    }

    // Loading indicator pass. Visible pages that did NOT get a kitty
    // placement on this frame fall into two buckets and BOTH leave
    // an empty rectangle that confuses the user:
    //   1. Rapid-burst defer: blit exists with placement_active=false.
    //      We have its dst geometry from the original visible-page
    //      loop and use it directly.
    //   2. Bitmap missing: pdfium hasn't rendered this page yet, so
    //      the visible-page loop bailed out at `pixel_dims = None`
    //      (no blit entry at all). Recompute the strip from layout.
    // Paint a centered "loading page N…" line into the cleared area
    // so the user sees pending work, not a glitch.
    {
        use std::collections::HashSet;
        let placed: HashSet<usize> = blits
            .iter()
            .filter(|b| b.placement_active)
            .map(|b| b.page_idx)
            .collect();
        let scroll_y = app.scroll_y_px;
        for &page_idx in &visible {
            if placed.contains(&page_idx) {
                continue;
            }
            paint_loading_indicator(
                buf,
                area,
                app,
                page_idx,
                scroll_y,
                cell_w,
                cell_h,
                area_height_cells,
            );
        }
    }

    // Now that the buffer has the placements, mark each page (and
    // overlay) as transmitted so the next frame skips the transmit if
    // state is unchanged. Then bound the registry size: evict LRU
    // non-visible pages so a 700-page PDF doesn't pile up gigabytes
    // of decoded RGBA in the terminal. The deletes queued here ride
    // out on the next frame's first transmit (see take_pending_deletes
    // above).
    let kp = app.kitty_pages.as_mut().unwrap();
    for b in &blits {
        if b.need_transmit {
            kp.mark_transmitted(b.page_idx, layout_key, b.revision, b.pixel_w, b.pixel_h);
        }
    }
    for (b, mark) in blits.iter().zip(overlay_marks.iter()) {
        if let Some((rev, sel_sig, w, h)) = *mark {
            kp.overlay_mark_transmitted(b.page_idx, layout_key, rev, sel_sig, w, h);
        }
    }
    kp.evict_to_budget(&visible_range_pin);

    // Link-hint overlay: rendered last so labels sit on top of
    // placeholder cells. Cells we paint here override ratatui's
    // skip-from-placement marker — that's fine because hints occupy
    // a 1-2 cell region and the rest of the page is unaffected.
    if app.link_hint_mode {
        draw_link_hints(f.buffer_mut(), app, area, cell_w, cell_h);
    }

    Ok(())
}

/// Paint each unfiltered hint label over its link's centre cell.
/// Black-on-yellow for visibility against arbitrary page content.
/// Paint a centered "loading page N…" line into the cleared cells of
/// a page strip. Called for visible pages that didn't get a kitty
/// placement this frame — either because the rapid-burst defer skipped
/// them or because pdfium hasn't rendered the bitmap yet.
///
/// Computes the page's on-screen y-row range from layout (no bitmap
/// needed) and writes a one-row label at the strip's vertical center.
fn paint_loading_indicator(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    app: &App<'_>,
    page_idx: usize,
    scroll_y: i64,
    cell_w: u32,
    cell_h: u32,
    area_height_cells: u16,
) {
    use ratatui::style::{Color, Modifier, Style};

    let page_doc_y = app.layout.page_y(page_idx);
    let page_h_px = app.layout.page_h(page_idx);
    if page_h_px == 0 {
        return;
    }
    let cell_h_safe = cell_h.max(1) as i64;
    // Strip top in cells, clamped to the visible area.
    let strip_top_doc = (page_doc_y - scroll_y).max(0);
    let strip_top_cell = (strip_top_doc / cell_h_safe).min(area_height_cells as i64) as u16;
    // Strip bottom = strip_top + page height, clamped to the area.
    let strip_bot_doc = ((page_doc_y + page_h_px as i64) - scroll_y).max(0);
    let strip_bot_cell =
        ((strip_bot_doc + cell_h_safe - 1) / cell_h_safe).min(area_height_cells as i64) as u16;
    let strip_height = strip_bot_cell.saturating_sub(strip_top_cell);
    if strip_height == 0 {
        return;
    }
    // Vertical center of the strip in absolute buffer coordinates.
    let mid_row = area
        .top()
        .saturating_add(strip_top_cell)
        .saturating_add(strip_height / 2);
    if mid_row >= area.bottom() {
        return;
    }

    // Label text and horizontal centering. Use the layout's fit width
    // so the label sits over where the page would render — same x-axis
    // alignment the kitty placement uses.
    let label = format!(" loading page {}… ", page_idx + 1);
    let label_chars: Vec<char> = label.chars().collect();
    let img_w_cells = (app.layout.fit_width_px / cell_w.max(1)) as u16;
    let dst_left_cell = if img_w_cells < area.width {
        (area.width - img_w_cells) / 2
    } else {
        0
    };
    let page_x_left = area.left().saturating_add(dst_left_cell);
    let page_width = img_w_cells.min(area.width.saturating_sub(dst_left_cell));
    if page_width == 0 || label_chars.len() as u16 > page_width {
        return;
    }
    let pad = (page_width - label_chars.len() as u16) / 2;
    let start_x = page_x_left.saturating_add(pad);

    let style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    let mut utf8_buf = [0u8; 4];
    for (i, ch) in label_chars.iter().enumerate() {
        let x = start_x.saturating_add(i as u16);
        if x >= area.right() {
            break;
        }
        if let Some(cell) = buf.cell_mut((x, mid_row)) {
            cell.set_symbol(ch.encode_utf8(&mut utf8_buf));
            cell.set_style(style);
            cell.set_skip(false);
        }
    }
}

fn draw_link_hints(
    buf: &mut ratatui::buffer::Buffer,
    app: &App<'_>,
    area: Rect,
    cell_w: u32,
    cell_h: u32,
) {
    use ratatui::style::{Color, Modifier, Style};
    let style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    let scroll_y = app.scroll_y_px;
    let viewport_h = app.viewport_px.1;

    for hint in &app.link_hints {
        // Only show hints whose label still matches what the user
        // has typed so far. Already-typed prefix is dimmed visually
        // by greying the matched portion.
        if !hint.label.starts_with(&app.hint_filter) {
            continue;
        }

        let page_doc_y = app.layout.page_y(hint.page_idx);
        let page_h_px = app.layout.page_h(hint.page_idx);
        if page_h_px == 0 {
            continue;
        }
        // Page-px coordinates of the link rect's top-left.
        let pixel_w = app.layout.fit_width_px;
        let link_x_px = (hint.rect.x * pixel_w as f32) as i64;
        let link_y_px = (hint.rect.y * page_h_px as f32) as i64;
        // Doc-y of the link's centre.
        let doc_y = page_doc_y + link_y_px;
        // Skip if the link is outside the viewport vertically.
        let dy_px = doc_y - scroll_y;
        if dy_px < 0 || dy_px >= viewport_h as i64 {
            continue;
        }
        let cell_y = area
            .top()
            .saturating_add((dy_px / cell_h as i64).max(0) as u16);

        // Center horizontally within the page area: the page may
        // have been centered if narrower than viewport.
        let img_w_cells = (pixel_w / cell_w.max(1)) as u16;
        let dst_left_cell = if img_w_cells < area.width {
            (area.width - img_w_cells) / 2
        } else {
            0
        };
        let cell_x = area
            .left()
            .saturating_add(dst_left_cell)
            .saturating_add((link_x_px / cell_w as i64).max(0) as u16);

        // Write the label chars across consecutive cells.
        let label = &hint.label;
        let mut utf8_buf = [0u8; 4];
        for (i, ch) in label.chars().enumerate() {
            let x = cell_x.saturating_add(i as u16);
            if x >= area.right() || cell_y >= area.bottom() {
                break;
            }
            if let Some(cell) = buf.cell_mut((x, cell_y)) {
                cell.set_symbol(ch.encode_utf8(&mut utf8_buf));
                cell.set_style(style);
                cell.set_skip(false);
            }
        }
    }
}

/// How many cell-rows of this page actually intersect the viewport
/// after cell-quantizing the scroll offset. Returns 0 for pages whose
/// pixel intersection rounds away — common at inter-page gaps.
///
/// Pre-render check used to filter `visible_pages` so we don't pay
/// pdfium + overlay + transmit cost for pages the user can't see.
fn visible_cell_height(
    layout: &crate::layout::PageLayout,
    scroll_y_px: i64,
    viewport_h_cells: u16,
    page_idx: usize,
    cell_h_px: u32,
) -> u16 {
    let cell_h = cell_h_px.max(1);
    let page_doc_y = layout.page_y(page_idx);
    let page_h_px = layout.page_h(page_idx);
    if page_h_px == 0 {
        return 0;
    }
    let src_top_px = (scroll_y_px - page_doc_y).max(0) as u32;
    let dst_top_px = (page_doc_y - scroll_y_px).max(0) as u32;
    let dst_top_cell = (dst_top_px / cell_h) as u16;
    let src_top_cell = (src_top_px / cell_h) as u16;
    let img_h_cells = (page_h_px / cell_h) as u16;
    let max_dst_rows = viewport_h_cells.saturating_sub(dst_top_cell);
    let max_src_rows = img_h_cells.saturating_sub(src_top_cell);
    max_dst_rows.min(max_src_rows)
}

/// Per-page revision: hash of every overlay-affecting field except
/// the layout (which is tracked separately by the kitty registry).
/// Bumping this for a page invalidates only that page's transmit
/// cache, leaving every other transmitted page in the terminal alone.
///
/// Pub(crate) so the idle warm path in main.rs can recompute the same
/// fingerprint when populating the pre-encode cache — both paths must
/// agree or the draw cycle will see a cache miss and re-encode anyway.
pub(crate) fn compute_page_revision(app: &App<'_>, page_idx: usize) -> u64 {
    let (search_revision, has_search_hits, current_hit_on_this_page) = match &app.search {
        Some(s) => {
            let any = s.page_has_hits(page_idx);
            let cur = s.current_hit().map(|h| h.page == page_idx).unwrap_or(false);
            (s.revision, any, cur)
        }
        None => (0u64, false, false),
    };
    // Per-page highlight fingerprint: changes only when *this* page's
    // highlight set changes. The `highlight_add` perf-harness scenario
    // went from 12 transmits per `y` keystroke to 1 once this localised.
    let highlight_sig = app.highlights.page_revision(page_idx);

    // Selection signature is included so a moving selection band
    // re-bakes the page bitmap. The earlier design split the band
    // into a separate kitty image at a classical placement (z=1),
    // which paints in absolute terminal coordinates and bypasses
    // tmux's per-pane clipping — adjacent panes went blank during
    // selection. tdf, yazi, and ranger all bake-into-page for the
    // same reason. Costs ~1 page re-encode per selection step (~2 ms
    // PNG fast-Up); buys correctness across tmux + Ghostty + kitty.
    let selection_sig = app.selection_signature_for_page(page_idx);

    // FNV-1a-style mix; doesn't need to be cryptographic — just
    // non-degenerate enough that flipping any single field flips the
    // revision.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in [
        highlight_sig,
        selection_sig,
        search_revision,
        has_search_hits as u64,
        current_hit_on_this_page as u64,
    ] {
        h ^= v;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Scroll-shift optimisation: when the only ComposeKey field that
/// changed since the last frame is `scroll_y_px`, and the absolute
/// delta is < viewport_h, reuse the previous canvas by `copy_within`-
/// shifting its rows by ΔY and repainting only the newly-exposed
/// strip (top or bottom). Saves all the per-page blit work for pages
/// that were already on the canvas at their new position.
///
/// Returns `None` when:
///   - this is the first frame (no previous compose_key)
///   - any non-scroll field of ComposeKey differs (layout change,
///     viewport resize, highlight edit, selection move)
///   - the canvas dims don't match (resize)
///   - |ΔY| ≥ viewport_h (full rebuild is cheaper than chasing
///     pages whose entire frame is now off-canvas)
///   - canvas_buf is missing
///
/// In any of those cases the caller falls back to the full compose
/// path, which still benefits from canvas reuse + the highlights-baked
/// tier.
fn try_scroll_shift_canvas(
    app: &mut App<'_>,
    new_key: &ComposeKey,
    viewport_w: u32,
    viewport_h: u32,
) -> Option<RgbaImage> {
    let prev = app.last_compose_key.as_ref()?;
    // Every field except scroll_y_px must match.
    if prev.layout != new_key.layout
        || prev.viewport_w != new_key.viewport_w
        || prev.viewport_h != new_key.viewport_h
        || prev.scroll_x_milli != new_key.scroll_x_milli
        || prev.highlight_revision != new_key.highlight_revision
        || prev.selection_sig != new_key.selection_sig
    {
        return None;
    }
    let dy = new_key.scroll_y_px - prev.scroll_y_px;
    if dy == 0 {
        // No-op scroll. Caller's hash-skip will catch the canvas
        // reuse; nothing to do here.
        return app.canvas_buf.take();
    }
    let abs_dy = dy.unsigned_abs() as usize;
    if abs_dy >= viewport_h as usize {
        return None;
    }
    let mut canvas = app.canvas_buf.take()?;
    if canvas.width() != viewport_w || canvas.height() != viewport_h {
        // Stale buffer from a previous size; force the fallback.
        return None;
    }

    let stride = (viewport_w as usize) * 4;
    shift_canvas_rows(canvas.as_mut(), viewport_w, viewport_h, dy);

    // Repaint only the freshly-exposed strip.
    let (strip_y0, strip_y1) = if dy > 0 {
        (viewport_h as i64 - abs_dy as i64, viewport_h as i64)
    } else {
        (0, abs_dy as i64)
    };

    {
        // Block-scope the App borrow so the rest of this function can
        // re-borrow `app` for layout/visible-pages reads.
        let bg_row = app.bg_row(viewport_w);
        let buf = canvas.as_mut();
        for y in strip_y0..strip_y1 {
            let off = (y as usize) * stride;
            buf[off..off + stride].copy_from_slice(bg_row);
        }
    }

    // Now blit the visible pages over the strip. The pages outside
    // the strip already have correct pixels (carried over by the
    // memmove). For pages that intersect the strip, we re-blit the
    // whole page; blit_clipped naturally clips the parts that are
    // outside the strip.
    let fit_width_px = app.layout.fit_width_px;
    let visible = app.layout.visible_pages(app.scroll_y_px, viewport_h);
    let page_x_origin: i64 = if fit_width_px <= viewport_w {
        ((viewport_w - fit_width_px) / 2) as i64
    } else {
        -(((fit_width_px - viewport_w) as f32) * app.scroll_x).round() as i64
    };
    for page_idx in visible {
        let page_doc_y = app.layout.page_y(page_idx);
        let page_h = app.layout.page_h(page_idx) as i64;
        let page_top_in_viewport = page_doc_y - app.scroll_y_px;
        let page_bot_in_viewport = page_top_in_viewport + page_h;
        // Skip pages that don't touch the exposed strip.
        if page_bot_in_viewport <= strip_y0 || page_top_in_viewport >= strip_y1 {
            continue;
        }
        let Some(page_img) = app.composed_image(page_idx) else {
            continue;
        };
        // Constrain the blit to the strip rows. Easiest: clip the
        // canvas writes by writing to a scratch view. Cheapest
        // implementation: just blit the whole page — pixels outside
        // the strip get rewritten with the same content they
        // already had (this page didn't move). Correctness guaranteed.
        blit_clipped(&mut canvas, page_x_origin, page_top_in_viewport, page_img);
    }

    Some(canvas)
}

/// Selection-motion fast path: when the only ComposeKey change is
/// `selection_sig`, the previous canvas is correct everywhere
/// EXCEPT the rows occupied by visible pages whose own
/// `selection_signature_for_page` changed. Reuse the previous
/// canvas, then re-blit just those pages.
///
/// This is the natural extension of try_scroll_shift_canvas to
/// in-place selection growth (`l`, `j`, `w` in Visual). It saves
/// the per-page blit + the gap fill for every page that didn't
/// change.
fn try_selection_only_repaint(
    app: &mut App<'_>,
    new_key: &ComposeKey,
    viewport_w: u32,
    viewport_h: u32,
) -> Option<RgbaImage> {
    let prev = app.last_compose_key.as_ref()?;
    if prev.layout != new_key.layout
        || prev.viewport_w != new_key.viewport_w
        || prev.viewport_h != new_key.viewport_h
        || prev.scroll_x_milli != new_key.scroll_x_milli
        || prev.scroll_y_px != new_key.scroll_y_px
        || prev.highlight_revision != new_key.highlight_revision
    {
        return None;
    }
    if prev.selection_sig == new_key.selection_sig {
        // No selection change either; the global compose-key check
        // already short-circuits here. Defensive return None.
        return None;
    }
    let mut canvas = app.canvas_buf.take()?;
    if canvas.width() != viewport_w || canvas.height() != viewport_h {
        return None;
    }

    let fit_width_px = app.layout.fit_width_px;
    let visible = app.layout.visible_pages(app.scroll_y_px, viewport_h);
    let page_x_origin: i64 = if fit_width_px <= viewport_w {
        ((viewport_w - fit_width_px) / 2) as i64
    } else {
        -(((fit_width_px - viewport_w) as f32) * app.scroll_x).round() as i64
    };

    // Re-blit any page whose per-page selection_sig changed since
    // the last compose. The selection always spans a contiguous
    // range, so the affected set is the union of the previous range
    // and the current one — pages that newly entered the selection,
    // pages that just left it, and the (single) page where the head
    // is currently moving inside its own range.
    //
    // Without the union, a shrink that pulls the selection's tail
    // off page N would leave page N's old selection band painted on
    // the canvas — `composed_image(N)` returns an unbanded bitmap
    // but we'd never re-blit it. The kitty path doesn't need this
    // because each page transmits independently and the per-page
    // revision already covers selection_sig=0; canvas mode reuses
    // one big bitmap across pages and so has to clear stale bands
    // explicitly.
    let now = pages_touched_by_selection(app);
    let prev = app.last_selection_range;
    let union = match (prev, now) {
        (None, None) => None,
        (Some(r), None) | (None, Some(r)) => Some(r),
        (Some((a, b)), Some((c, d))) => Some((a.min(c), b.max(d))),
    };
    let Some((lo, hi)) = union else {
        return Some(canvas);
    };
    for page_idx in visible {
        if page_idx < lo || page_idx > hi {
            continue;
        }
        let Some(page_img) = app.composed_image(page_idx) else {
            continue;
        };
        let page_doc_y = app.layout.page_y(page_idx);
        let page_y_in_viewport = page_doc_y - app.scroll_y_px;
        blit_clipped(&mut canvas, page_x_origin, page_y_in_viewport, page_img);
    }

    Some(canvas)
}

/// Inclusive `(lo_page, hi_page)` range the active selection spans.
/// Returns `None` when there is no selection. The selection always
/// covers a contiguous run of pages, so a tuple is sufficient — no
/// HashSet allocation on the per-frame hot path.
fn pages_touched_by_selection(app: &App<'_>) -> Option<(usize, usize)> {
    let sel = app.text_selection?;
    let (lo, hi) = sel.ordered();
    Some((lo.page, hi.page))
}

/// Pure helper: shift the rows of `buf` by `dy` viewport pixels.
/// `dy > 0` = scroll down (content moves UP on the canvas). `dy < 0`
/// = scroll up (content moves DOWN). Caller is responsible for
/// repainting the freshly-exposed strip; this only does the memmove.
///
/// Extracted so the row-shift arithmetic has a regression test that
/// doesn't need an App or a full render pipeline.
pub fn shift_canvas_rows(buf: &mut [u8], viewport_w: u32, viewport_h: u32, dy: i64) {
    if dy == 0 {
        return;
    }
    let stride = (viewport_w as usize) * 4;
    let abs_dy = dy.unsigned_abs() as usize;
    if abs_dy >= viewport_h as usize {
        return;
    }
    if dy > 0 {
        let src_start = abs_dy * stride;
        let src_end = (viewport_h as usize) * stride;
        buf.copy_within(src_start..src_end, 0);
    } else {
        let src_end = ((viewport_h as usize) - abs_dy) * stride;
        let dst = abs_dy * stride;
        buf.copy_within(0..src_end, dst);
    }
}

/// FNV-1a-style 64-bit over the raw RGBA bytes. Used purely as an
/// "are these pixels the same" check, so we can deviate from canonical
/// FNV byte-at-a-time mixing in favour of mixing 8 bytes per iteration.
/// Net effect on an 8 MB canvas: ~6× wall-time win because the inner
/// loop becomes a simple u64 xor + multiply over `chunks_exact(8)`.
/// Collision risk is unchanged — different inputs still mix into
/// different output paths; cryptographic strength was never needed.
///
/// Stable across runs (seed-free), so the canvas-hash cache key
/// behaves the same across reopens and across cold/warm caches.
fn fnv1a_hash(bytes: &[u8]) -> u64 {
    const FNV_PRIME: u64 = 0x100_0000_01b3;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let chunks = bytes.chunks_exact(8);
    let tail = chunks.remainder();
    for c in chunks {
        // SAFETY: chunks_exact(8) guarantees a length-8 slice.
        let v = u64::from_le_bytes(c.try_into().unwrap());
        h ^= v;
        h = h.wrapping_mul(FNV_PRIME);
    }
    for &b in tail {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Drain completed render-worker responses into `page_cache` so
/// prefetched pages become available to the very next frame. Called
/// at the top of `ensure_image`. Cheap: a non-blocking `try_recv`
/// loop, returns instantly when the channel is empty.
///
/// Stale renders (key no longer matches the current `LayoutKey` —
/// the user zoomed or toggled dark mode while the page was in
/// flight) are dropped on the floor. The next ensure_image cycle
/// will re-issue with the new dimensions.
fn drain_worker_results(app: &mut App<'_>) {
    let Some(worker) = app.render_worker.as_ref() else {
        return;
    };
    let current_layout = LayoutKey {
        fit_width_px: app.layout.fit_width_px,
        dark: app.dark,
    };
    let drained = worker.drain();
    for resp in drained {
        app.pages_in_flight
            .remove(&(resp.req.page, resp.req.target_width_px, resp.req.dark));
        if resp.req.target_width_px != current_layout.fit_width_px
            || resp.req.dark != current_layout.dark
        {
            // Stale — layout moved on while this was in flight.
            continue;
        }
        match resp.image {
            Ok(img) => {
                app.page_cache.insert(resp.req.page, img);
                app.touch_page(resp.req.page);
                // Force a recompose since a previously-blank page
                // now has pixels.
                app.last_compose_key = None;
            }
            Err(_) => {
                // Worker hit an error. Mark the page as failed so we
                // don't keep retrying every frame.
                app.failed_pages.insert(resp.req.page);
            }
        }
    }
}

/// Send a prefetch request to the background worker. No-op if:
///   - the worker isn't available (failed to spawn or already
///     disconnected),
///   - the page is already cached,
///   - the page is already in flight at the requested dimensions,
///   - the page is on the failed-pages blacklist.
/// Falls back to nothing — prefetch is best-effort.
fn request_prefetch(app: &mut App<'_>, page_idx: usize, fit_width_px: u32) {
    if app.page_cache.contains_key(&page_idx) {
        return;
    }
    if app.failed_pages.contains(&page_idx) {
        return;
    }
    let key = (page_idx, fit_width_px, app.dark);
    if app.pages_in_flight.contains(&key) {
        return;
    }
    let Some(worker) = app.render_worker.as_ref() else {
        // No worker — fall back to synchronous prefetch (the old
        // behaviour). Failures swallowed.
        let _ = ensure_page_rendered(app, page_idx, fit_width_px, true);
        return;
    };
    let req = crate::render_worker::RenderReq {
        page: page_idx,
        target_width_px: fit_width_px,
        dark: app.dark,
    };
    if worker.request(req) {
        app.pages_in_flight.insert(key);
    } else {
        // Worker died; clear it so future calls take the fallback path.
        app.render_worker = None;
        let _ = ensure_page_rendered(app, page_idx, fit_width_px, true);
    }
}

/// Render `page_idx` through pdfium if it's not already cached.
/// Honours `App::failed_pages` so a corrupt page isn't re-attempted
/// every frame. With `allow_failure=true`, errors are stored and
/// then suppressed (returning Ok); with `allow_failure=false`, the
/// error propagates so the caller can paint a render-error message.
///
/// Disk cache: tries to load a previously-cached PNG of this exact
/// (page, width, dark) tuple from `~/.cache/termpdf-rs/<file-hash>/`
/// before invoking pdfium. On miss, runs pdfium and writes the result
/// for next time. Saves ~15 ms per cold page on warm-cache reopens.
pub(crate) fn ensure_page_rendered(
    app: &mut App<'_>,
    page_idx: usize,
    fit_width_px: u32,
    allow_failure: bool,
) -> Result<()> {
    if app.page_cache.contains_key(&page_idx) {
        return Ok(());
    }
    if app.failed_pages.contains(&page_idx) {
        return Ok(());
    }

    // Disk-cache fast path: hit avoids the pdfium render entirely.
    // Disk only stores Sharp-quality renders, so a hit means we get
    // the highest-fidelity image for free — no upgrade needed.
    let cache_path = app
        .cache_dir
        .as_deref()
        .map(|d| crate::disk_cache::cache_path_in_dir(d, page_idx, fit_width_px, app.dark));
    if let Some(ref p) = cache_path {
        if let Some(img) = crate::disk_cache::load(p) {
            if img.width() == fit_width_px {
                app.page_cache.insert(page_idx, img);
                app.pages_at_fast_quality.remove(&page_idx);
                app.last_compose_key = None;
                return Ok(());
            }
        }
    }

    // No disk hit → render at Fast quality (~6-10 ms vs ~25-40 ms
    // for Sharp). Disk write is DEFERRED — the idle upgrade path
    // re-renders this page at Sharp later and only then writes to
    // disk, so the scroll hot path never pays for PNG encode + atomic
    // file write. The user reported a 50→74°C scroll-induced spike
    // on a 600-page book; this is the half of the fix that lands on
    // the keystroke path.
    match pdf::render_page_at_width(&app.document, page_idx, fit_width_px) {
        Ok(img) => {
            let img = if app.dark {
                DynamicImage::ImageRgba8(dark::invert_luminance(img))
            } else {
                img
            };
            app.page_cache.insert(page_idx, img);
            app.pages_at_fast_quality.insert(page_idx);
            app.last_compose_key = None;
            Ok(())
        }
        Err(e) => {
            app.failed_pages.insert(page_idx);
            app.status = format!("page {}: render failed", page_idx + 1);
            if allow_failure {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

/// Re-render `page_idx` at `RenderQuality::Sharp`, replacing the
/// existing Fast-quality entry and persisting the result to the disk
/// cache. Invalidates the per-page baked + overlay caches since their
/// source bitmap just changed. Called from the idle path so the heat
/// of the upgrade lands when the user's hands aren't moving.
///
/// Returns `Ok(true)` if an upgrade actually happened, `Ok(false)`
/// if the page wasn't a Fast-quality candidate, and propagates render
/// errors otherwise.
pub(crate) fn upgrade_page_to_sharp(
    app: &mut App<'_>,
    page_idx: usize,
    fit_width_px: u32,
) -> Result<bool> {
    if !app.pages_at_fast_quality.contains(&page_idx) {
        return Ok(false);
    }
    if app.failed_pages.contains(&page_idx) {
        return Ok(false);
    }
    let img = match pdf::render_page_sharp(&app.document, page_idx, fit_width_px) {
        Ok(i) => i,
        Err(_) => {
            // Don't poison `failed_pages` — the Fast version is still
            // usable. Just leave the Fast entry and try again next idle.
            return Ok(false);
        }
    };
    let img = if app.dark {
        DynamicImage::ImageRgba8(dark::invert_luminance(img))
    } else {
        img
    };
    if let Some(d) = app.cache_dir.as_deref() {
        let p = crate::disk_cache::cache_path_in_dir(d, page_idx, fit_width_px, app.dark);
        let _ = crate::disk_cache::store(&p, &img);
    }
    app.page_cache.insert(page_idx, img);
    app.pages_at_fast_quality.remove(&page_idx);
    // Source pixels changed — drop the derived per-page caches and
    // force the kitty registry to re-transmit. Neither revision nor
    // layout key changes on a Fast→Sharp upgrade, so without
    // explicit invalidation the next is_fresh would return true and
    // the terminal would keep showing the Fast pixels forever.
    app.highlights_baked_cache.remove(&page_idx);
    app.overlay_cache.remove(&page_idx);
    if let Some(kp) = app.kitty_pages.as_mut() {
        kp.invalidate_transmit(page_idx);
    }
    app.last_compose_key = None;
    Ok(true)
}

fn page_overlay_key(app: &App<'_>, page_idx: usize, layout: LayoutKey) -> PageOverlayKey {
    let (search_revision, has_search_hits, current_hit_on_this_page) = match &app.search {
        Some(s) => {
            let any = s.page_has_hits(page_idx);
            let cur = s.current_hit().map(|h| h.page == page_idx).unwrap_or(false);
            (s.revision, any, cur)
        }
        None => (0, false, false),
    };
    PageOverlayKey {
        layout,
        // Per-page highlight fingerprint — see compute_page_revision for
        // the why. Localising this prevents an edit on page 5 from
        // forcing a re-bake of the overlay tier on every visible page.
        highlight_revision: app.highlights.page_revision(page_idx),
        search_revision,
        has_search_hits,
        current_hit_on_this_page,
        selection_sig: app.selection_signature_for_page(page_idx),
    }
}

/// Build (or refresh) the cached overlay bitmap for `page_idx`.
/// Two-tier:
///   1. **highlights_baked**: page bitmap with saved highlights and
///      search hits blended in. Keyed without selection_sig, so it
///      survives every Visual-mode keystroke.
///   2. **overlay_cache**: highlights_baked + the live selection
///      band. Keyed with selection_sig so it rebuilds on motion.
///
/// Net win: on selection move we no longer re-blend the saved
/// highlights / search hits; we clone the highlights_baked image
/// and paint just the selection band on top. For a heavily
/// highlighted page the saved-N×fill_rect_blend disappears.
pub(crate) fn ensure_overlay(app: &mut App<'_>, page_idx: usize, layout: LayoutKey) {
    // Both kitty + non-kitty backends now bake the selection band
    // into the page bitmap. The earlier "kitty short-circuit" used a
    // separate classical-placement overlay (`a=p,U=0,z=1`) for the
    // band — but classical placements paint at absolute terminal
    // coordinates that tmux can't clip per-pane, so dragging a
    // selection blanked adjacent Ghostty panes (multiple kitty/tmux
    // discussions confirm this is architecturally unfixable for
    // U=0 placements). tdf, yazi, ranger all bake-into-page for
    // the same reason. Cost: ~1 page re-encode per selection step
    // (~2 ms PNG fast-Up); buys correctness across tmux + multi-pane.

    let overlay_key = page_overlay_key(app, page_idx, layout);
    if app
        .overlay_cache
        .get(&page_idx)
        .map(|(_, k)| *k == overlay_key)
        .unwrap_or(false)
    {
        return;
    }

    // Tier 1: highlights baked. Built once per (highlights, search,
    // layout) change and reused across every selection move.
    ensure_highlights_baked(app, page_idx, layout);

    // Fast path: when the active selection doesn't touch this page,
    // the overlay would just be a verbatim clone of `baked`. Drop any
    // stale overlay_cache entry (e.g. selection just moved off this
    // page) and skip the clone — `App::composed_image` falls back to
    // `highlights_baked_cache` for pages without an overlay entry.
    // This keeps overlay_cache memory proportional to the *selection
    // span* instead of all visible pages, and saves a ~10 MB RGBA
    // copy on every cold compose of a non-selection page.
    if overlay_key.selection_sig == 0 {
        app.overlay_cache.remove(&page_idx);
        return;
    }

    // Reuse the previous overlay buffer if its dimensions match. Saves
    // an 8 MB malloc/free per Visual-mode keystroke that hits a
    // selection-touching page: `baked.clone()` allocates a fresh
    // ~8 MB Vec and copies, while a reused buffer only does the copy.
    // On dim mismatch (zoom changed) we fall back to clone and the
    // old buffer's storage drops naturally.
    let prev_buf = app.overlay_cache.remove(&page_idx).map(|(b, _)| b);
    let Some((baked, _)) = app.highlights_baked_cache.get(&page_idx) else {
        return;
    };
    let mut img = match prev_buf {
        Some(mut existing) if existing.dimensions() == baked.dimensions() => {
            let dst: &mut [u8] = existing.as_mut();
            dst.copy_from_slice(baked.as_raw());
            existing
        }
        _ => baked.clone(),
    };

    // Tier 2: paint the live selection band on top.
    if let Some(sel) = app.text_selection {
        if let Some(pt) = app.text_cache.get(page_idx) {
            bake_selection_into_page(
                &mut img,
                page_idx,
                sel,
                pt,
                app.selection_color_idx,
                app.selection_placement,
            );
        }
    }

    app.overlay_cache.insert(page_idx, (img, overlay_key));
}

/// Build the highlights-baked tier for `page_idx` if its key changed.
/// Touches the per-highlight loop and the per-search-hit loop —
/// both of which used to run on every Visual-mode keystroke. Now
/// they only run when the highlights/search state itself changes.
fn ensure_highlights_baked(app: &mut App<'_>, page_idx: usize, layout: LayoutKey) {
    let (search_revision, has_search_hits, current_hit_on_this_page) = match &app.search {
        Some(s) => {
            let any = s.page_has_hits(page_idx);
            let cur = s.current_hit().map(|h| h.page == page_idx).unwrap_or(false);
            (s.revision, any, cur)
        }
        None => (0, false, false),
    };
    let highlight_revision = app.highlights.page_revision(page_idx);

    // Cold-page no-overlay fast path: if this page has nothing painted
    // (no highlights, no search hits, no current-hit outline), the
    // bake would just clone the page bitmap and return. Skip the
    // ~6 MB clone and leave the cache empty for this page; consumers
    // (`draw_pages_kitty`, the warm tick) have a fallback to
    // `page_cache.as_rgba8()` for pages without a baked entry.
    //
    // Net win on `scroll_casual_large`: every cold page hits this
    // branch (the test PDF has no highlights / search) so the per-
    // scroll RGBA clone disappears. cpu_ms drops accordingly.
    let no_overlays = highlight_revision == 0 && !has_search_hits;
    if no_overlays {
        // Drop any stale entry (e.g. user just deleted the last
        // highlight on this page) so consumers fall through to
        // page_cache. The page_cache is the source of truth in this
        // state — keeping a cached clone would diverge if the user
        // re-renders at a different quality (Fast → Sharp upgrade).
        app.highlights_baked_cache.remove(&page_idx);
        return;
    }

    let key = HighlightsBakedKey {
        layout,
        // Per-page fingerprint — saved-highlights tier no longer
        // re-bakes for every visible page on a single-page edit.
        highlight_revision,
        search_revision,
        has_search_hits,
        current_hit_on_this_page,
    };
    if app
        .highlights_baked_cache
        .get(&page_idx)
        .map(|(_, k)| *k == key)
        .unwrap_or(false)
    {
        return;
    }
    let Some(src) = app.page_cache.get(&page_idx) else {
        return;
    };
    // Reuse the prior baked buffer when dims match. The bake always
    // overwrites every pixel from `src` first, so there's no risk of
    // stale-paint bleed-through. Saves an 8 MB malloc/free per
    // highlight-revision bump (e.g. selecting the next search hit
    // re-bakes every visible page). When `src` isn't ImageRgba8 the
    // borrow falls back to a `to_rgba8` conversion clone — pdfium
    // always hands us ImageRgba8 in practice, so the fallback only
    // exists to keep the function total.
    let prev_buf = app.highlights_baked_cache.remove(&page_idx).map(|(b, _)| b);
    let mut img = match (src.as_rgba8(), prev_buf) {
        (Some(src_rgba), Some(mut existing)) if existing.dimensions() == src_rgba.dimensions() => {
            let dst: &mut [u8] = existing.as_mut();
            dst.copy_from_slice(src_rgba.as_raw());
            existing
        }
        (Some(src_rgba), _) => src_rgba.clone(),
        (None, _) => src.to_rgba8(),
    };

    for h in app.highlights.for_page(page_idx) {
        let rect = norm_to_pixels(
            Rect01 {
                x: h.x,
                y: h.y,
                w: h.w,
                h: h.h,
            },
            img.width(),
            img.height(),
        );
        let rgb = rgb_from_hex(&h.color);
        fill_rect_blend(&mut img, rect, rgb, 0.35);
    }

    if let Some(s) = &app.search {
        if let Some((start, slice)) = s.page_slice(page_idx) {
            let current_idx = s.current;
            for (offset, hit) in slice.iter().enumerate() {
                let rect = norm_to_pixels(hit.rect, img.width(), img.height());
                let color = (255u8, 165, 0);
                fill_rect_blend(&mut img, rect, color, 0.45);
                if start + offset == current_idx {
                    outline_rect(&mut img, rect, (255, 80, 0), 3);
                }
            }
        }
    }

    app.highlights_baked_cache.insert(page_idx, (img, key));
}

/// Paint the viewport canvas: stitch the cached overlay bitmap of
/// every visible page into a single RgbaImage. Reuses the buffer
/// from `app.canvas_buf` when its dimensions match — saves the
/// 8 MB allocator round-trip + memset every recompose at 1080p.
///
/// To avoid leaking last-frame pixels through the inter-page gaps,
/// we paint the bg colour only into the rows NOT covered by any
/// visible page (and into any horizontal margin if the page is
/// narrower than the viewport). Page bodies overwrite themselves,
/// so the costly full memset is replaced by a few targeted row
/// fills. For zoomed pages narrower than the viewport, the side
/// margins also get filled.
fn compose_into_buffer(app: &mut App<'_>, viewport_w: u32, viewport_h: u32) -> RgbaImage {
    let bg = if app.dark {
        Rgba([20, 20, 20, 255])
    } else {
        Rgba([240, 240, 240, 255])
    };

    // Pull/recreate the canvas buffer.
    let mut canvas = match app.canvas_buf.take() {
        Some(c) if c.width() == viewport_w && c.height() == viewport_h => c,
        _ => RgbaImage::from_pixel(viewport_w, viewport_h, bg),
    };

    let fit_width_px = app.layout.fit_width_px;
    let scroll_y = app.scroll_y_px;
    let visible = app.layout.visible_pages(scroll_y, viewport_h);

    let page_x_origin: i64 = if fit_width_px <= viewport_w {
        ((viewport_w - fit_width_px) / 2) as i64
    } else {
        -(((fit_width_px - viewport_w) as f32) * app.scroll_x).round() as i64
    };

    // Build a list of (start_row, end_row) extents that the visible
    // pages cover in the viewport. Then fill the inverse with bg.
    let mut covered: Vec<(i64, i64)> = Vec::with_capacity(visible.len());
    for page_idx in visible.clone() {
        let page_doc_y = app.layout.page_y(page_idx);
        let page_h = app.layout.page_h(page_idx) as i64;
        let top = (page_doc_y - scroll_y).max(0);
        let bot = (page_doc_y - scroll_y + page_h).min(viewport_h as i64);
        if bot > top {
            covered.push((top, bot));
        }
    }

    {
        // Block-scope the bg_row borrow so the rest of compose can
        // re-borrow `app` for layout/overlay reads below. Reuse the
        // same cached bg_row for the side margins so we get a single
        // copy_from_slice per row instead of a chunks_exact_mut loop
        // populating one pixel quad at a time.
        let bg_row = app.bg_row(viewport_w);
        fill_gaps_bulk(canvas.as_mut(), viewport_w, viewport_h, &covered, bg_row);
        if fit_width_px < viewport_w {
            fill_side_margins(
                &mut canvas,
                viewport_w,
                viewport_h,
                page_x_origin,
                fit_width_px,
                bg_row,
            );
        }
    }

    for page_idx in visible {
        let Some(page_img) = app.composed_image(page_idx) else {
            continue;
        };
        let page_doc_y = app.layout.page_y(page_idx);
        let page_y_in_viewport = page_doc_y - scroll_y;
        blit_clipped(&mut canvas, page_x_origin, page_y_in_viewport, page_img);
    }

    canvas
}

/// Paint `bg_row` into every row of `buf` that lies in a *gap*
/// between adjacent covered intervals (or before the first / after
/// the last). `covered` must be sorted top-to-bottom; overlapping
/// or touching intervals are coalesced as we go.
///
/// Replaces a 1080-iteration row scan (every viewport row checked
/// against every covered range) with a 1-3 iteration pass over the
/// gap intervals — a typical scroll position covers exactly one
/// page top-to-bottom, so the gap list is empty or trivial. The
/// memcpy bandwidth is unchanged; the bookkeeping cost drops to
/// nearly zero.
fn fill_gaps_bulk(
    buf: &mut [u8],
    viewport_w: u32,
    viewport_h: u32,
    covered: &[(i64, i64)],
    bg_row: &[u8],
) {
    let row_bytes = (viewport_w as usize) * 4;
    let viewport_h_i = viewport_h as i64;
    let mut cursor: i64 = 0;
    for &(top, bot) in covered {
        if top > cursor {
            let gap_start = cursor.max(0);
            let gap_end = top.min(viewport_h_i);
            for y in gap_start..gap_end {
                let off = (y as usize) * row_bytes;
                buf[off..off + row_bytes].copy_from_slice(bg_row);
            }
        }
        if bot > cursor {
            cursor = bot;
        }
    }
    if cursor < viewport_h_i {
        let gap_start = cursor.max(0);
        for y in gap_start..viewport_h_i {
            let off = (y as usize) * row_bytes;
            buf[off..off + row_bytes].copy_from_slice(bg_row);
        }
    }
}

/// Paint side margins (cells outside the page's horizontal extent)
/// with the cached background row. Only called when the page is
/// narrower than the viewport.
///
/// Each row's left and right margins are filled with one
/// `copy_from_slice` from `bg_row` instead of a per-quad
/// `chunks_exact_mut` loop. `bg_row` is the same buffer
/// `compose_into_buffer` already prepared for `fill_gaps_bulk`, so
/// no extra setup cost.
fn fill_side_margins(
    canvas: &mut RgbaImage,
    viewport_w: u32,
    viewport_h: u32,
    page_x_origin: i64,
    fit_width_px: u32,
    bg_row: &[u8],
) {
    let left_end = page_x_origin.max(0).min(viewport_w as i64) as usize;
    let right_start = (page_x_origin + fit_width_px as i64)
        .max(0)
        .min(viewport_w as i64) as usize;
    let viewport_w_usize = viewport_w as usize;
    let buf = canvas.as_mut();
    let row_bytes = viewport_w_usize * 4;
    let left_bytes = left_end * 4;
    let right_start_bytes = right_start * 4;
    for y in 0..viewport_h as usize {
        let row_off = y * row_bytes;
        let row = &mut buf[row_off..row_off + row_bytes];
        if left_end > 0 {
            row[..left_bytes].copy_from_slice(&bg_row[..left_bytes]);
        }
        if right_start < viewport_w_usize {
            row[right_start_bytes..].copy_from_slice(&bg_row[right_start_bytes..]);
        }
    }
}

/// Blit `src` onto `dst` at position `(dst_x, dst_y)`, clipping to
/// the destination's bounds. Row-wise `copy_from_slice` over the
/// raw RGBA buffer; `image::imageops::overlay` would do alpha
/// blending we don't need (overlay bitmaps are opaque). Coordinates
/// are signed so a partially off-screen src (top of first visible
/// page above the viewport) is clipped instead of panicking.
fn blit_clipped(dst: &mut RgbaImage, dst_x: i64, dst_y: i64, src: &RgbaImage) {
    let dw = dst.width() as i64;
    let dh = dst.height() as i64;
    let sw = src.width() as i64;
    let sh = src.height() as i64;

    let sx0 = (-dst_x).max(0);
    let sy0 = (-dst_y).max(0);
    let sx1 = (dw - dst_x).min(sw);
    let sy1 = (dh - dst_y).min(sh);
    if sx1 <= sx0 || sy1 <= sy0 {
        return;
    }

    let dst_w = dst.width() as usize;
    let src_w = src.width() as usize;
    let row_bytes = ((sx1 - sx0) as usize) * 4;
    let dst_buf = dst.as_mut();
    let src_buf = src.as_raw();
    for sy in sy0..sy1 {
        let src_off = (sy as usize) * src_w * 4 + (sx0 as usize) * 4;
        let dst_y_row = (dst_y + sy) as usize;
        let dst_x_off = (dst_x + sx0) as usize;
        let dst_off = dst_y_row * dst_w * 4 + dst_x_off * 4;
        dst_buf[dst_off..dst_off + row_bytes]
            .copy_from_slice(&src_buf[src_off..src_off + row_bytes]);
    }
}

/// Bake the active Visual-mode selection's contribution to one page
/// into its overlay bitmap. Pure: takes the page's text layout, the
/// selection state, and the chosen palette index, mutates `img`
/// in-place. Extracted from `ensure_overlay` so it can be regression-
/// tested with synthetic inputs (no pdfium, no terminal, no `App`).
///
/// Layout/colour invariants this function guarantees, asserted by
/// `bake_selection_paints_visible_band` in tests:
///  * On a page covered by the selection, at least one pixel inside
///    the selection's pixel rect ends up shifted toward the palette
///    colour.
///  * Pixels outside the selection's bbox are untouched.
///  * The caret bbox lands inside the band (so the user sees where
///    their cursor is).
///  * On pages outside `[lo.page, hi.page]`, the bitmap is unchanged.
pub fn bake_selection_into_page(
    img: &mut RgbaImage,
    page_idx: usize,
    sel: crate::textlayout::TextSelection,
    pt: &crate::textlayout::PageText,
    color_idx: usize,
    placement_mode: bool,
) {
    let (lo, hi) = sel.ordered();
    if !(lo.page..=hi.page).contains(&page_idx) {
        return;
    }
    // In placement mode the user is positioning the caret — don't
    // paint the (single-char) band fill; it would look like a tiny
    // yellow blob and falsely suggest text is already selected.
    // Just draw the caret accent so it looks like a cursor.
    if !placement_mode {
        let mut start = if page_idx == lo.page { lo.idx } else { 0 };
        let mut end = if page_idx == hi.page {
            hi.idx
        } else {
            pt.chars.len().saturating_sub(1)
        };
        if matches!(sel.mode, crate::textlayout::SelMode::Linewise) {
            if let Some(line) = pt.line_of(start) {
                if let Some(s) = pt.line_start(line) {
                    start = s;
                }
            }
            if let Some(line) = pt.line_of(end) {
                if let Some(e) = pt.line_end(line) {
                    end = e;
                }
            }
        }
        let color = HIGHLIGHT_COLORS[color_idx % HIGHLIGHT_COLORS.len()];
        for r01 in pt.range_to_rects(start, end) {
            let rect = norm_to_pixels(r01, img.width(), img.height());
            fill_rect_blend(img, rect, color.rgb, 0.45);
        }
    }
    if page_idx == sel.head.page {
        if let Some(c) = pt.chars.get(sel.head.idx) {
            let mut caret_rect = norm_to_pixels(c.bbox, img.width(), img.height());
            if caret_rect.2 < 2 {
                caret_rect.2 = 2;
            }
            if caret_rect.3 < 2 {
                caret_rect.3 = 2;
            }
            outline_rect(img, caret_rect, (40, 40, 40), 1);
            fill_rect_blend(img, caret_rect, (255, 255, 255), 0.55);
        }
    }
}

// The cell-overlay path (paint `▒` cells over the kitty image area)
// used to live here. It was unreliable in tmux+Ghostty: ratatui-image
// packs each image row's escape sequence into column 0 of the row and
// our subsequent overlay writes never reached the wire because the
// renderer's diff loop advances past those cells via the symbol-width
// counter. The selection now bakes into the page bitmap (see
// `ensure_overlay`) so it travels through the same kitty re-upload
// path that already works for saved highlights.

/// Percent through the document, based on doc-pixel scroll position.
/// 0% = top of doc; 100% = bottom of last page in viewport. Pure
/// helper so it can be unit-tested without an `App` instance.
pub(crate) fn reading_percent(app: &App<'_>) -> u32 {
    reading_percent_pure(
        app.scroll_y_px,
        app.viewport_px.1 as i64,
        app.layout.total_height_px,
    )
}

pub(crate) fn reading_percent_pure(scroll_y_px: i64, viewport_h_px: i64, total_h_px: i64) -> u32 {
    let scrollable = (total_h_px - viewport_h_px).max(1);
    let p = (scroll_y_px.max(0) as f64 / scrollable as f64) * 100.0;
    p.clamp(0.0, 100.0).round() as u32
}

fn status_line(app: &App<'_>) -> Paragraph<'static> {
    // Up to ~7 spans: mode label, page/percent, DARK, zoom, pending,
    // status, help hint. Pre-size so the per-frame status line build
    // doesn't grow through the 4/8/16 doubling steps.
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(7);

    let mode_label = match app.mode {
        Mode::Normal => "",
        Mode::Visual => " VISUAL ",
        Mode::Command => ":",
        Mode::Search => "/",
    };

    if !mode_label.is_empty() {
        spans.push(Span::styled(
            mode_label,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if matches!(app.mode, Mode::Command | Mode::Search) {
        spans.push(Span::raw(app.cmd_buffer.clone()));
    } else {
        // Reading-progress indicator. Shows page index plus % through
        // the document — handy on long books where "page 234" is
        // meaningless without context. The percent uses the *visual*
        // top-of-viewport position (not page index) so scrolling
        // mid-page nudges it smoothly.
        let pct = reading_percent(app);
        spans.push(Span::styled(
            format!(
                " {}/{}  {:>2}%  ",
                app.current_page() + 1,
                app.page_count,
                pct
            ),
            Style::default().fg(Color::White),
        ));
        if app.dark {
            spans.push(Span::styled("DARK  ", Style::default().fg(Color::Cyan)));
        }
        if (app.zoom - 1.0).abs() > 0.001 {
            spans.push(Span::raw(format!("zoom {:.0}%  ", app.zoom * 100.0)));
        }
        if !app.pending.is_empty() {
            spans.push(Span::styled(
                format!("[{}] ", app.pending),
                Style::default().fg(Color::Yellow),
            ));
        }
        if !app.status.is_empty() {
            spans.push(Span::styled(
                app.status.clone(),
                Style::default().fg(Color::DarkGray),
            ));
        }
        spans.push(Span::styled(
            "    ?  for help",
            Style::default().fg(Color::DarkGray),
        ));
    }

    Paragraph::new(Line::from(spans))
}

fn draw_toc(f: &mut Frame, app: &mut App<'_>, area: Rect) {
    use ratatui::style::Stylize;

    let panel_w = area.width.saturating_mul(2) / 5;
    let panel_w = panel_w.clamp(40, 80).min(area.width);
    let panel_h = area.height;
    let popup = Rect {
        x: area.x + area.width.saturating_sub(panel_w),
        y: area.y,
        width: panel_w,
        height: panel_h,
    };

    f.render_widget(Clear, popup);

    let filtered = app.toc_filtered_indices();
    let inner_w = popup.width.saturating_sub(2) as usize;
    let body_h = popup.height.saturating_sub(2) as usize;

    // Scroll offset: keep the cursor visible.
    let cursor = app.toc_cursor;
    let scroll = cursor.saturating_sub(body_h.saturating_sub(1));

    let mut lines: Vec<Line> = Vec::with_capacity(body_h);
    if filtered.is_empty() {
        let msg = if app.toc_filter.is_empty() {
            "(no entries)"
        } else {
            "(no matches)"
        };
        lines.push(Line::from(Span::styled(
            msg.to_string(),
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (display_idx, &entry_idx) in filtered.iter().enumerate().skip(scroll).take(body_h) {
            let entry = &app.outline[entry_idx];
            let text = crate::outline::render_line(entry, inner_w);
            let style = if display_idx == cursor {
                Style::default().fg(Color::Black).bg(Color::Yellow)
            } else if entry.page.is_none() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
    }

    let title = if app.toc_filter_editing {
        format!(" outline · /{}_ ", app.toc_filter)
    } else if !app.toc_filter.is_empty() {
        format!(" outline · /{} ", app.toc_filter)
    } else {
        " outline (j/k Enter · / filter · Esc close) ".to_string()
    };
    let para =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title.bold()));
    f.render_widget(para, popup);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let help_lines = help_overlay_lines();
    let h = (help_lines.len() as u16 + 4).min(area.height);
    let w = 68u16.min(area.width);
    let popup = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    let para = Paragraph::new(
        help_lines
            .into_iter()
            .map(|s| Line::from(s.to_string()))
            .collect::<Vec<_>>(),
    )
    .block(Block::default().borders(Borders::ALL).title(" ? help "));
    f.render_widget(para, popup);
}

/// The full text of the `?` help overlay, exposed as a pure list so
/// tests can assert specific entries are present (and so the README
/// key table can be cross-checked against the in-app help by anyone
/// who wants to write that test).
pub fn help_overlay_lines() -> Vec<&'static str> {
    vec![
        "termpdf-rs — continuous-scroll PDF reader",
        "",
        "  j / k                  next / prev page (jump to page boundary)",
        "  N j  /  N k            jump N pages forward / back",
        "  Space / b              scroll one screen down / up (less-style)",
        "  Ctrl-d / Ctrl-u        scroll a half-screen down / up",
        "  gg / G                 doc top / bottom",
        "  N G                    jump to page N",
        "",
        "  Up / Down              scroll vertically in fine steps",
        "  Left / Right / h / l   scroll horizontally (only when zoomed past fit-width)",
        "  mouse wheel            scroll (Shift = horizontal)",
        "",
        "  + / - / 0              zoom in / out / reset",
        "  d                      toggle dark mode (luminance-only)",
        "",
        "  v                      Visual mode — placement first (caret moves freely)",
        "    h j k l              move caret by char / line (no selection yet)",
        "    v  again             lock anchor → motions now grow the selection",
        "    v  third time        unlock anchor → caret moves freely again",
        "    w / b / e            next / prev word start / word end",
        "    0 / ^ / $            line start / first non-blank / line end",
        "    gg / G               first / last char on this page",
        "    f<c> / F<c>          jump to next/prev <c> on this line",
        "    iw / is / ip         select inner word / sentence / paragraph",
        "    V                    switch to linewise selection",
        "    Ctrl-v               switch to blockwise (rectangular)",
        "    c                    cycle highlight color",
        "    y / Enter            save highlight + copy text",
        "    Y                    copy text (no highlight)",
        "    gy                   copy as Markdown blockquote with citation",
        "    Esc                  cancel",
        "  click + drag           highlight with the mouse",
        "  x  then  y             delete last highlight on current page",
        "                         (whole multi-line group; press anything else to cancel)",
        "",
        "  m{a-z} / '{a-z}        set / jump to mark (persisted per PDF)",
        "  Ctrl-o / Ctrl-i        jumplist back / forward (Tab also forward)",
        "",
        "  /<query>               search the document",
        "  n / N                  next / previous match",
        "  :nohl                  clear search results",
        "",
        "  ]] / [[                jump to next / prev outline entry",
        "  :refs / :bib           jump to References / Bibliography section",
        "  o  /  :toc             open outline panel",
        "    j/k Enter            navigate / jump to entry",
        "    / type Enter         filter by substring",
        "    Esc                  close panel",
        "",
        "  f                      link-hint mode (vimium-style)",
        "                         type the 1-2 char label over a link to follow it",
        "                         internal links jump pages; URLs open via xdg-open",
        "                         Esc cancels",
        "  :<n>  /  :goto N       jump to page n",
        "  :export [path]         dump highlights as Markdown notes",
        "  :info                  show PDF metadata (title, author, page count, size)",
        "  :diag                  show terminal + render diagnostics (cell px, fit_w, scale)",
        "  :q                     quit",
        "  :set dark | :set nodark",
        "  ?                      toggle this overlay",
        "  q                      quit",
        "",
        "Press ? or Esc to close",
    ]
}

#[cfg(test)]
mod cold_render_plan_tests {
    use super::{plan_cold_render, ColdRenderDecision, MAX_COLD_RENDERS_PER_DRAW};

    #[test]
    fn cached_page_is_always_already_cached() {
        // Cache hit dominates over rapid + budget — the early return
        // prevents the render loop from charging a cold-budget slot
        // for pages that don't need any pdfium work.
        assert_eq!(
            plan_cold_render(true, false, 0),
            ColdRenderDecision::AlreadyCached
        );
        assert_eq!(
            plan_cold_render(true, true, 0),
            ColdRenderDecision::AlreadyCached
        );
        assert_eq!(
            plan_cold_render(true, true, 5),
            ColdRenderDecision::AlreadyCached
        );
    }

    #[test]
    fn cold_with_budget_renders() {
        assert_eq!(
            plan_cold_render(false, false, 1),
            ColdRenderDecision::Render
        );
        assert_eq!(
            plan_cold_render(false, false, MAX_COLD_RENDERS_PER_DRAW),
            ColdRenderDecision::Render
        );
    }

    #[test]
    fn cold_under_rapid_scroll_defers_as_rapid() {
        // Rapid-scroll defer staggers cold work until input settles.
        // Must stay defer even when budget would otherwise allow it
        // — held-`j` autorepeat should not punch through into pdfium
        // calls. The `Rapid` flavour is critical: it tells the caller
        // NOT to set `pending_cold_redraw` (would loop at 60 Hz).
        assert_eq!(
            plan_cold_render(false, true, 1),
            ColdRenderDecision::DeferRapid
        );
        assert_eq!(
            plan_cold_render(false, true, MAX_COLD_RENDERS_PER_DRAW),
            ColdRenderDecision::DeferRapid
        );
    }

    #[test]
    fn cold_with_zero_budget_defers_as_budget() {
        // The crash-bait case: a `100G` jump puts 3+ visible pages all
        // cold at once. After the first one renders, budget hits 0 and
        // the rest must defer to next-frame catch-up. The `Budget`
        // flavour means the caller SHOULD set `pending_cold_redraw`
        // so the deferred page renders at +16 ms.
        assert_eq!(
            plan_cold_render(false, false, 0),
            ColdRenderDecision::DeferBudget
        );
    }

    #[test]
    fn budget_constant_within_safe_burst_window() {
        // Compile-time guard: keep the per-draw cold-render cap small
        // enough that 3 visible pages take ≥3 draw frames to catch up,
        // i.e. ≥48 ms at 60 Hz. Anything bigger reopens the Ghostty
        // window-vanish hazard.
        const _: () = assert!(MAX_COLD_RENDERS_PER_DRAW >= 1);
        const _: () = assert!(MAX_COLD_RENDERS_PER_DRAW <= 2);
    }
}

#[cfg(test)]
mod transmit_budget_plan_tests {
    use super::{plan_transmit_deferrals, MAX_TRANSMITS_PER_DRAW};

    #[test]
    fn under_budget_no_deferrals() {
        // 2 pages, budget 2 → nothing to shed. Both transmit same frame.
        let blits = vec![(true, 0, true), (true, 1, true)];
        assert!(plan_transmit_deferrals(&blits, 0, 2).is_empty());
    }

    #[test]
    fn over_budget_drops_farthest_from_current() {
        // 4 visible pages all stale, current page = 1, budget = 2.
        // Page 1 (dist 0) and page 0 or 2 (dist 1) survive; page 3 dropped.
        let blits = vec![
            (true, 0, true),
            (true, 1, true),
            (true, 2, true),
            (true, 3, true),
        ];
        let to_defer = plan_transmit_deferrals(&blits, 1, 2);
        assert_eq!(to_defer.len(), 2);
        // Closest two indices (1, 0 or 2 by tie) survive.
        assert!(to_defer.contains(&3));
    }

    #[test]
    fn pages_without_prior_transmit_never_deferred() {
        // Mixed: page 0 (stale, has prior), page 5 (cold first-time, no prior).
        // Even if page 5 is far, we can't defer it — placement without prior
        // bytes shows garbled cells. Budget = 1 → page 0 gets deferred instead.
        let blits = vec![
            (true, 0, true),  // far from current
            (true, 5, false), // first-time, MUST transmit
        ];
        let to_defer = plan_transmit_deferrals(&blits, 5, 1);
        // Only 1 deferrable; non-deferrable consumes the budget — defer all 1.
        assert_eq!(to_defer, vec![0]);
    }

    #[test]
    fn no_need_transmit_blits_ignored() {
        // Pages with need_transmit=false aren't part of the budget calc.
        let blits = vec![
            (false, 0, true),
            (false, 1, true),
            (false, 2, true),
            (true, 3, true),
        ];
        assert!(plan_transmit_deferrals(&blits, 0, 1).is_empty());
    }

    #[test]
    fn all_non_deferrable_returns_empty() {
        // Every transmit is first-time (no prior). Budget=1 but 3 pages
        // need to ship; deferring any would garble. Helper returns empty;
        // upstream cold-render budget should have prevented this state.
        let blits = vec![(true, 0, false), (true, 1, false), (true, 2, false)];
        assert!(plan_transmit_deferrals(&blits, 1, 1).is_empty());
    }

    #[test]
    fn budget_zero_defers_everything_deferrable() {
        // Pathological case: budget = 0. All deferrable transmits drop.
        let blits = vec![(true, 0, true), (true, 1, true)];
        let to_defer = plan_transmit_deferrals(&blits, 0, 0);
        assert_eq!(to_defer.len(), 2);
    }

    #[test]
    fn budget_constant_within_safe_burst_window() {
        // Compile-time guard. Keep the cap small enough that ≥3 stale
        // visible pages stagger over ≥2 frames; anything larger
        // reopens the multi-MB-burst hazard the cold-render fix
        // already addressed.
        const _: () = assert!(MAX_TRANSMITS_PER_DRAW >= 1);
        const _: () = assert!(MAX_TRANSMITS_PER_DRAW <= 3);
    }
}

#[cfg(test)]
mod tests {
    //! Visual / cell-buffer tests for the parts of the renderer that
    //! don't depend on a live `App` (and therefore on pdfium).
    //!
    //! For each widget we either:
    //! - Inspect the pure builder's output (Line/Style spans), OR
    //! - Render through ratatui's `TestBackend` and walk the resulting
    //!   buffer cell-by-cell to assert the chars + styles that would
    //!   actually hit a real terminal.
    //!
    //! These catch the class of regression where a refactor silently
    //! changes a color, a glyph, or the help-overlay copy.
    use super::*;
    use crate::textlayout::{Caret, CharCell, LineSpan, PageText, SelMode, TextSelection};
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    // ---- Scroll-shift fast path -------------------------------------

    /// Building block: rows shift, exposed strip is left untouched
    /// for the caller to repaint. Use a tiny 4×4 buffer where each
    /// pixel's R channel encodes its original row index, so the
    /// shift result is trivial to verify.
    #[test]
    fn shift_canvas_rows_scrolls_content_down_then_up() {
        // 4 rows × 1 column-equivalent (pretend cell is 4 bytes/row).
        let w = 1u32;
        let h = 4u32;
        let stride = 4usize;
        let mut buf = vec![0u8; (h as usize) * stride];
        for y in 0..h {
            // R = row index, G/B/A = 0/0/255 so we can identify each row.
            let off = (y as usize) * stride;
            buf[off] = y as u8;
            buf[off + 3] = 255;
        }

        // dy = +1: scroll down ⇒ content moves UP. Row 0 should now
        // hold what was at row 1, row 1 ← row 2, row 2 ← row 3.
        // Row 3 is the exposed strip — caller will overwrite, but
        // we're not testing that here.
        shift_canvas_rows(&mut buf, w, h, 1);
        assert_eq!(buf[0], 1, "row 0 should hold ex-row-1");
        assert_eq!(buf[stride], 2, "row 1 should hold ex-row-2");
        assert_eq!(buf[2 * stride], 3, "row 2 should hold ex-row-3");

        // Reset.
        for y in 0..h {
            let off = (y as usize) * stride;
            buf[off] = y as u8;
        }
        // dy = -2: scroll up by 2. Content moves DOWN by 2 rows.
        // Row 2 ← row 0, row 3 ← row 1. Rows 0,1 are exposed.
        shift_canvas_rows(&mut buf, w, h, -2);
        assert_eq!(buf[2 * stride], 0, "row 2 should hold ex-row-0");
        assert_eq!(buf[3 * stride], 1, "row 3 should hold ex-row-1");
    }

    /// `dy == 0` and `|dy| >= viewport_h` are both no-ops (the latter
    /// because the entire canvas would be exposed and a memmove is
    /// pointless). The buffer must be unchanged in both cases.
    #[test]
    fn shift_canvas_rows_no_op_at_boundaries() {
        let w = 1u32;
        let h = 4u32;
        let stride = 4usize;
        let mut buf = vec![0u8; (h as usize) * stride];
        for y in 0..h {
            buf[(y as usize) * stride] = y as u8;
        }
        let snapshot = buf.clone();

        shift_canvas_rows(&mut buf, w, h, 0);
        assert_eq!(buf, snapshot, "dy=0 must leave buffer untouched");

        shift_canvas_rows(&mut buf, w, h, 4);
        assert_eq!(
            buf, snapshot,
            "dy >= viewport_h must leave buffer untouched"
        );

        shift_canvas_rows(&mut buf, w, h, -4);
        assert_eq!(
            buf, snapshot,
            "|dy| >= viewport_h must leave buffer untouched"
        );
    }

    // ---- Selection bake (the real "is the user seeing it" test) ----

    /// Build a synthetic 1-page text layout with two short lines of
    /// 5 chars each so we can drive `bake_selection_into_page`
    /// without needing pdfium or a real PDF.
    fn synthetic_page_text() -> PageText {
        // Two horizontal lines: y in [0.10, 0.20] and [0.40, 0.50].
        // Five chars per line, evenly spaced over x in [0.05, 0.55].
        let mut chars = Vec::new();
        for line in 0..2 {
            let y_top = if line == 0 { 0.10 } else { 0.40 };
            let y_bot = y_top + 0.10;
            for col in 0..5 {
                let x = 0.05 + (col as f32) * 0.10;
                chars.push(CharCell {
                    idx: chars.len(),
                    ch: Some(if line == 0 { 'a' } else { 'b' }),
                    bbox: Rect01 {
                        x,
                        y: y_top,
                        w: 0.08,
                        h: y_bot - y_top,
                    },
                    line,
                    origin_x: x,
                    is_generated: false,
                });
            }
        }
        let lines = vec![
            LineSpan {
                y_top: 0.10,
                y_bot: 0.20,
                start_idx: 0,
                end_idx: 4,
                word_starts: vec![0],
            },
            LineSpan {
                y_top: 0.40,
                y_bot: 0.50,
                start_idx: 5,
                end_idx: 9,
                word_starts: vec![5],
            },
        ];
        PageText {
            page_idx: 0,
            chars,
            lines,
            width_pts: 100.0,
            height_pts: 100.0,
        }
    }

    /// Pixels at the centre of a normalised rect on a `(w, h)` image.
    fn center_pixel(r: Rect01, w: u32, h: u32) -> (u32, u32) {
        let cx = ((r.x + r.w / 2.0) * w as f32) as u32;
        let cy = ((r.y + r.h / 2.0) * h as f32) as u32;
        (cx.min(w - 1), cy.min(h - 1))
    }

    /// REGRESSION: the user reported "I cannot see the selection" even
    /// after the cell-overlay skip-flag fix, because tmux+Ghostty's
    /// kitty pipeline drops post-image cell writes. We switched to
    /// baking the selection band INTO the page bitmap instead of
    /// drawing it as cells. This test proves the bake function
    /// actually paints colored pixels on the bitmap so the user sees
    /// something change when they enter Visual mode. Without it, a
    /// silent regression to "selection not painted" would only be
    /// caught by a human eyeballing the terminal.
    #[test]
    fn bake_selection_paints_visible_band_on_page_bitmap() {
        use image::{Rgba, RgbaImage};

        // 200×200 white page. Bake a charwise selection covering the
        // first three chars on line 0 (x ∈ [0.05, 0.31], y ∈ [0.10, 0.20]).
        let mut img = RgbaImage::from_pixel(200, 200, Rgba([255, 255, 255, 255]));
        let pt = synthetic_page_text();
        let sel = TextSelection {
            anchor: Caret { page: 0, idx: 0 },
            head: Caret { page: 0, idx: 2 },
            mode: SelMode::Charwise,
        };

        bake_selection_into_page(&mut img, 0, sel, &pt, 0 /* yellow */, false);

        // Yellow palette colour is (0xff, 0xd5, 0x4f). Blended at 0.45
        // over white gives (255, ~244, ~204) — green channel must
        // drop below 250 and blue below 230 in the band region.
        let (px, py) = center_pixel(pt.chars[1].bbox, 200, 200);
        let p = img.get_pixel(px, py).0;
        assert!(
            p[1] < 250 && p[2] < 230,
            "expected yellow tint at band centre ({px},{py}), got {p:?}"
        );

        // Pixels well outside the selection's vertical band must be
        // untouched (top of page, line 1 on Y=0.40+, etc.).
        let outside = img.get_pixel(100, 60).0; // y=0.30, between line 0 and 1
        assert_eq!(
            [outside[0], outside[1], outside[2]],
            [255, 255, 255],
            "pixel outside the selection band should still be white"
        );
    }

    /// The caret accent (dark outline + white fill) must land at the
    /// head so the user can see where their cursor is. We compare
    /// pixels at the head bbox vs a non-head char in the same band:
    /// the head must be visually distinct (it's white-tinted on top
    /// of the yellow band; the rest of the band is just yellow).
    ///
    /// Catches: caret not painted at all (no-op fix), caret drawn
    /// at the anchor instead of head, caret drawn off-screen.
    #[test]
    fn bake_selection_paints_caret_inside_band_at_head() {
        use image::{Rgba, RgbaImage};
        let mut img = RgbaImage::from_pixel(200, 200, Rgba([255, 255, 255, 255]));
        let pt = synthetic_page_text();
        let sel = TextSelection {
            anchor: Caret { page: 0, idx: 0 },
            head: Caret { page: 0, idx: 3 },
            mode: SelMode::Charwise,
        };

        bake_selection_into_page(&mut img, 0, sel, &pt, 0, false);

        // Yellow band at 0.45 alpha over white = (255, 244, 204).
        // White-tinted caret over that band at 0.55 alpha + dark
        // outline => head pixels look noticeably brighter than
        // surrounding band cells (R≈255, G,B much higher than the
        // band's ~244/~204).
        let (head_x, head_y) = center_pixel(pt.chars[3].bbox, 200, 200);
        let (band_x, band_y) = center_pixel(pt.chars[1].bbox, 200, 200);
        let head_p = img.get_pixel(head_x, head_y).0;
        let band_p = img.get_pixel(band_x, band_y).0;
        // saturating_add so the no-bake case (both pure white) fails
        // here cleanly instead of panicking on integer overflow.
        assert!(
            head_p[2] > band_p[2].saturating_add(20),
            "head caret should look brighter (whiter) than the band: \
             head={head_p:?} vs band={band_p:?}. \
             Caret accent isn't being painted at the head."
        );

        // The anchor (idx 0) must look like the rest of the band —
        // no extra brightening. Catches the regression where the
        // caret follows the anchor instead of the head.
        let (anchor_x, anchor_y) = center_pixel(pt.chars[0].bbox, 200, 200);
        let anchor_p = img.get_pixel(anchor_x, anchor_y).0;
        assert!(
            anchor_p[2] <= band_p[2].saturating_add(5),
            "anchor pixel should match the rest of the band: \
             anchor={anchor_p:?} vs band={band_p:?}. \
             Caret is following the anchor, not the head."
        );
    }

    /// Bake on a page outside the selection's [lo.page, hi.page]
    /// range must be a no-op. Otherwise paging through a doc with an
    /// active selection would keep painting bands on every page.
    #[test]
    fn bake_selection_is_noop_on_pages_outside_selection() {
        use image::{Rgba, RgbaImage};
        let mut img = RgbaImage::from_pixel(200, 200, Rgba([255, 255, 255, 255]));
        let pt = synthetic_page_text();
        let sel = TextSelection {
            anchor: Caret { page: 5, idx: 0 },
            head: Caret { page: 5, idx: 2 },
            mode: SelMode::Charwise,
        };

        // Page 0 is outside [5, 5]. Bitmap must come back unchanged.
        bake_selection_into_page(&mut img, 0, sel, &pt, 0, false);
        for y in [10, 50, 100, 150, 199] {
            for x in [10, 50, 100, 150, 199] {
                let p = img.get_pixel(x, y).0;
                assert_eq!(
                    [p[0], p[1], p[2]],
                    [255, 255, 255],
                    "non-selected page should be untouched at ({x},{y})"
                );
            }
        }
    }


    /// In placement mode the bake must NOT paint a band fill — the
    /// user is positioning the caret, not selecting yet. We verify
    /// the band-area pixels stay white (untouched by `fill_rect_blend`)
    /// while the caret-area pixels still get the white-tinted accent.
    /// Without this skip, a single-char "tiny yellow blob" would
    /// suggest text is already selected.
    #[test]
    fn bake_in_placement_mode_paints_caret_but_not_band() {
        use image::{Rgba, RgbaImage};
        let mut img = RgbaImage::from_pixel(200, 200, Rgba([255, 255, 255, 255]));
        let pt = synthetic_page_text();
        // anchor == head means placement mode's "caret only" intent.
        let caret = Caret { page: 0, idx: 2 };
        let sel = TextSelection {
            anchor: caret,
            head: caret,
            mode: SelMode::Charwise,
        };

        bake_selection_into_page(&mut img, 0, sel, &pt, 0, /*placement=*/ true);

        // Pick a pixel inside the char's bbox but OUTSIDE the caret
        // accent (which sits centred). The caret's outline+white blend
        // is one pixel thick on each border + a translucent fill;
        // even there the colour is not pure yellow. The band fill
        // alone would tint the whole bbox (255, 244, 204). We assert
        // there is NO tinted-yellow region anywhere on the bitmap —
        // i.e. no pixel with that telltale (low-blue, high-green-red)
        // combo which only fill_rect_blend at the band step produces.
        let mut yellow_band_pixels = 0;
        for y in 0..200u32 {
            for x in 0..200u32 {
                let p = img.get_pixel(x, y).0;
                // Yellow band signature: R=255, G≈244, B≈204.
                if p[0] == 255 && (240..=250).contains(&p[1]) && (200..=220).contains(&p[2]) {
                    yellow_band_pixels += 1;
                }
            }
        }
        assert_eq!(
            yellow_band_pixels, 0,
            "placement mode should not paint the yellow band fill — \
             found {yellow_band_pixels} band-tinted pixels"
        );

        // The caret accent must still be there so the user can see
        // where their cursor is.
        let (cx, cy) = center_pixel(pt.chars[2].bbox, 200, 200);
        let p = img.get_pixel(cx, cy).0;
        assert!(
            p[0] >= 250 && p[1] >= 250 && p[2] >= 250,
            "caret centre should be white-tinted in placement mode, got {p:?}"
        );
        // …and at least a few pixels inside the bbox that aren't pure
        // white — the caret fill (translucent white over the page)
        // and the outline (gray after the fill blends over it) both
        // produce sub-255 components.
        let head = &pt.chars[2].bbox;
        let x0 = (head.x * 200.0) as u32;
        let y0 = (head.y * 200.0) as u32;
        let x1 = ((head.x + head.w) * 200.0) as u32;
        let y1 = ((head.y + head.h) * 200.0) as u32;
        let mut non_white = 0;
        for y in y0..y1.min(200) {
            for x in x0..x1.min(200) {
                let p = img.get_pixel(x.min(199), y.min(199)).0;
                if p[0] != 255 || p[1] != 255 || p[2] != 255 {
                    non_white += 1;
                }
            }
        }
        assert!(
            non_white >= 4,
            "placement mode must still draw the caret accent inside the head's bbox \
             (found {non_white} non-white pixels)"
        );
    }

    // The selection-overlay-style tests used to live here. They tested
    // the cell-overlay code path, which has been removed in favour of
    // baking the selection band into the page bitmap (where the
    // `compose` integration tests cover it via real RGBA images).

    // ---- Help overlay text ------------------------------------------

    #[test]
    fn help_overlay_documents_all_documented_keys() {
        let lines = help_overlay_lines();
        let body = lines.join("\n");
        // Spot-check every keybinding the README also documents. If a
        // future refactor renames or drops one of these, BOTH the
        // help and this test should change in the same PR.
        for needed in [
            "j / k",
            "Space / b",
            "Ctrl-d / Ctrl-u",
            "gg / G",
            "m{a-z} / '{a-z}",
            "Ctrl-o / Ctrl-i",
            "v",
            "iw / is / ip",
            "y / Enter",
            "gy",
            "x  then  y",
            "/<query>",
            ":nohl",
            "o  /  :toc",
            ":export",
            ":q",
            "?",
        ] {
            assert!(
                body.contains(needed),
                "help overlay missing entry: {needed:?}"
            );
        }
    }

    #[test]
    fn help_overlay_renders_within_a_terminal_height() {
        // Pop the help in a minimum-size area; it should not panic
        // and the popup geometry should clamp to fit. Smoke test for
        // the area math (caught popup-overflow bug previously).
        let backend = TestBackend::new(50, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            draw_help(
                f,
                Rect {
                    x: 0,
                    y: 0,
                    width: 50,
                    height: 20,
                },
            );
        })
        .unwrap();
        // Walk the buffer and confirm the help title appears
        // somewhere — proves the popup actually rendered.
        let buf = term.backend().buffer();
        let mut content = String::new();
        for y in 0..20 {
            for x in 0..50 {
                content.push_str(buf.cell((x, y)).unwrap().symbol());
            }
            content.push('\n');
        }
        assert!(
            content.contains("? help"),
            "help title not rendered: contents=\n{content}"
        );
    }

    // ---- visible_cell_height -----------------------------------------

    #[test]
    fn visible_cell_height_zero_when_below_source_grid() {
        use crate::layout::PageLayout;
        use crate::pdf::PageMetrics;
        // Two 100×100 pages with no gap. Cell height = 16 px.
        // 100 / 16 = 6 source cells per page (last 4 px lost to
        // cell-quantization on purpose).
        let m = vec![
            PageMetrics {
                width_pts: 100.0,
                height_pts: 100.0,
            },
            PageMetrics {
                width_pts: 100.0,
                height_pts: 100.0,
            },
        ];
        let l = PageLayout::build(&m, 100, 0);
        // Scroll to y=96: src_top_cell=6 ≥ img_h_cells=6, so page 0
        // has no source cells left to draw — function must return 0.
        // (Pixel-wise 4 px of page 0 still intersect the viewport,
        // but cell quantization rounds those away.)
        let h0 = visible_cell_height(
            &l, 96, /*viewport_h_cells*/ 50, 0, /*cell_h_px*/ 16,
        );
        assert_eq!(h0, 0, "page 0 should have no source cells past row 96");
        let h1 = visible_cell_height(&l, 96, 50, 1, 16);
        assert!(h1 > 0, "page 1 should be visible");
    }

    #[test]
    fn visible_cell_height_positive_for_top_of_doc() {
        use crate::layout::PageLayout;
        use crate::pdf::PageMetrics;
        let m = vec![PageMetrics {
            width_pts: 100.0,
            height_pts: 100.0,
        }];
        let l = PageLayout::build(&m, 100, 0);
        let h = visible_cell_height(
            &l, 0, /*viewport_h_cells*/ 10, 0, /*cell_h_px*/ 16,
        );
        // 100 px / 16 px/cell = 6 cells; viewport allows 10, so 6 wins.
        assert_eq!(h, 6);
    }

    // ---- reading_percent_pure ---------------------------------------

    #[test]
    fn reading_percent_zero_at_top_of_doc() {
        // scroll=0, viewport=600, total=12000 → 0%.
        assert_eq!(reading_percent_pure(0, 600, 12000), 0);
    }

    #[test]
    fn reading_percent_hundred_at_bottom() {
        // scroll positions the viewport bottom flush with doc bottom.
        // total - viewport = 11400; scroll = 11400 → 100%.
        assert_eq!(reading_percent_pure(11400, 600, 12000), 100);
    }

    #[test]
    fn reading_percent_midway_lands_at_fifty() {
        // Half-scrolled through the scrollable distance → 50%.
        assert_eq!(reading_percent_pure(5700, 600, 12000), 50);
    }

    #[test]
    fn reading_percent_clamps_above_100() {
        // Past-end scroll (race window during a layout swap) must
        // not show 101% or panic.
        assert_eq!(reading_percent_pure(99999, 600, 12000), 100);
    }

    #[test]
    fn reading_percent_negative_scroll_treated_as_top() {
        // Defensive: a negative scroll_y_px (shouldn't happen in
        // practice, but the layout has saturating math) maps to 0%.
        assert_eq!(reading_percent_pure(-50, 600, 12000), 0);
    }

    #[test]
    fn reading_percent_short_doc_no_div_by_zero() {
        // Doc shorter than the viewport — scrollable distance would be
        // ≤0 if not floored at 1. Result must be in [0,100], not NaN
        // or a panic.
        let p = reading_percent_pure(0, 1000, 200);
        assert!(p <= 100);
        // With nothing to scroll the only sensible position is 0%.
        assert_eq!(p, 0);
    }

    #[test]
    fn reading_percent_doc_exactly_one_viewport_tall() {
        // total == viewport → no scroll possible → 0%.
        assert_eq!(reading_percent_pure(0, 600, 600), 0);
    }

    // ---- fill_gaps_bulk ---------------------------------------------

    fn make_buf(viewport_w: u32, viewport_h: u32, fill: u8) -> Vec<u8> {
        vec![fill; (viewport_w * viewport_h * 4) as usize]
    }

    #[test]
    fn fill_gaps_bulk_no_pages_fills_entire_viewport() {
        // Empty covered → every row is a gap, every byte becomes bg.
        let bg_row = [9u8; 4 * 4]; // 4 px wide, 4 bytes each
        let mut buf = make_buf(4, 3, 0);
        fill_gaps_bulk(&mut buf, 4, 3, &[], &bg_row);
        assert!(buf.iter().all(|&b| b == 9), "every byte should be bg");
    }

    #[test]
    fn fill_gaps_bulk_one_full_page_no_fills() {
        // Single covered region spans the whole viewport → nothing
        // to fill; buffer must be untouched.
        let bg_row = [0xAAu8; 4 * 4];
        let mut buf = make_buf(4, 5, 0x33);
        fill_gaps_bulk(&mut buf, 4, 5, &[(0, 5)], &bg_row);
        assert!(buf.iter().all(|&b| b == 0x33), "no rows should be filled");
    }

    #[test]
    fn fill_gaps_bulk_fills_top_strip_below_first_page() {
        // Page covers rows 1..3; rows 0 and 3,4 are gaps and must
        // become bg. Use distinct bg byte (0x77) so we can tell.
        let bg_row = [0x77u8; 4 * 4];
        let mut buf = make_buf(4, 5, 0x11);
        fill_gaps_bulk(&mut buf, 4, 5, &[(1, 3)], &bg_row);
        let row_bytes = 4 * 4usize;
        // Row 0 is a top gap.
        assert!(
            buf[0..row_bytes].iter().all(|&b| b == 0x77),
            "row 0 should be bg"
        );
        // Rows 1,2 are covered → untouched.
        assert!(
            buf[row_bytes..3 * row_bytes].iter().all(|&b| b == 0x11),
            "covered rows 1..3 should be untouched"
        );
        // Rows 3,4 are bottom gap → bg.
        assert!(
            buf[3 * row_bytes..].iter().all(|&b| b == 0x77),
            "rows 3..5 should be bg"
        );
    }

    #[test]
    fn fill_gaps_bulk_fills_inter_page_gap() {
        // Two pages with a gap row between them: covered = (0,2) +
        // (3,5); row 2 is the gap.
        let bg_row = [0x88u8; 4 * 4];
        let mut buf = make_buf(4, 5, 0x44);
        fill_gaps_bulk(&mut buf, 4, 5, &[(0, 2), (3, 5)], &bg_row);
        let row_bytes = 4 * 4usize;
        // Rows 0,1: covered.
        assert!(
            buf[0..2 * row_bytes].iter().all(|&b| b == 0x44),
            "rows 0,1 untouched"
        );
        // Row 2: inter-page gap → bg.
        assert!(
            buf[2 * row_bytes..3 * row_bytes].iter().all(|&b| b == 0x88),
            "row 2 should be bg"
        );
        // Rows 3,4: covered.
        assert!(
            buf[3 * row_bytes..].iter().all(|&b| b == 0x44),
            "rows 3,4 untouched"
        );
    }

    #[test]
    fn fill_gaps_bulk_handles_covered_past_viewport_end() {
        // Covered range extends past viewport_h — function must
        // clamp without OOB and still fill the leading gap.
        let bg_row = [0x55u8; 4 * 4];
        let mut buf = make_buf(4, 4, 0x22);
        fill_gaps_bulk(&mut buf, 4, 4, &[(2, 999)], &bg_row);
        let row_bytes = 4 * 4usize;
        assert!(
            buf[0..2 * row_bytes].iter().all(|&b| b == 0x55),
            "rows 0,1 should be bg (top gap)"
        );
        // The clamped covered range covers rows 2..4 in the viewport.
        assert!(
            buf[2 * row_bytes..].iter().all(|&b| b == 0x22),
            "covered rows 2,3 untouched"
        );
    }

    #[test]
    fn fill_gaps_bulk_handles_overlapping_covered() {
        // Pathological input: overlapping covered intervals (shouldn't
        // happen in practice, but layout glitches musn't crash). The
        // function must coalesce and only fill the genuinely-uncovered
        // rows. Result should match (0, 4) coverage = no gap.
        let bg_row = [0x99u8; 4 * 4];
        let mut buf = make_buf(4, 4, 0x33);
        fill_gaps_bulk(&mut buf, 4, 4, &[(0, 3), (1, 4)], &bg_row);
        assert!(
            buf.iter().all(|&b| b == 0x33),
            "overlapping covered must coalesce → no gap"
        );
    }
}
