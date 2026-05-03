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
use crate::compose::{fill_rect_blend, norm_to_pixels, outline_rect};
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

/// Soft byte budget on `App::page_cache`. A 4-byte-per-pixel RGBA
/// budget; 256 MB ≈ 64 megapixels of cached pages, which is several
/// dozen typical pages or a smaller number of big scanned ones.
/// Override at startup with `$TERMPDF_CACHE_MB`.
pub fn page_cache_budget_bytes() -> usize {
    let mb = std::env::var("TERMPDF_CACHE_MB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(256);
    mb.saturating_mul(1024 * 1024)
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
        if let Err(e) = draw_pages_kitty(f, app, img_area) {
            f.render_widget(
                Paragraph::new(format!("render error: {e:#}"))
                    .style(Style::default().fg(Color::Red)),
                img_area,
            );
        }
    } else if let Err(e) = ensure_image(app, img_area) {
        f.render_widget(
            Paragraph::new(format!("render error: {e:#}"))
                .style(Style::default().fg(Color::Red)),
            img_area,
        );
    } else if let Some(proto) = app.image_proto.as_mut() {
        // Non-kitty fallback (sixel / iterm2 / halfblocks): the
        // composed image is exactly viewport-sized (we already
        // centered/cropped while painting), so render it across the
        // full image area.
        f.render_stateful_widget(
            StatefulImage::<StatefulProtocol>::new(),
            img_area,
            proto,
        );
    }

    // The active Visual-mode selection is now baked into the page
    // bitmap by `ensure_overlay`, so it travels through the kitty
    // re-upload path that already works for saved highlights.
    // The cell-overlay we used to paint here was unreliable in
    // tmux+Ghostty: ratatui-image packs each row's kitty escape into
    // column 0 and our post-image cell writes never reached the wire.

    // Resume-reading marker: a single dim row of `─` at the position
    // where the viewport bottom was *before* the last `<Space>` jump.
    // Suppressed while a popup is up so the marker doesn't bleed at
    // the popup edges.
    if !app.show_help && !app.show_toc {
        draw_resume_marker(f, app, img_area);
    }

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

/// Paint the "resume reading here" marker if one is active and
/// within the current viewport. Expires the marker if its TTL has
/// elapsed. The marker lives in document-pixel space so zoom and
/// scroll changes leave it where the user expects; if it has scrolled
/// out of view we just skip drawing (it'll reappear if the user
/// scrolls back within its TTL).
///
/// Layout: occupies the LEFT 30% of the image area (so the rest of
/// the row stays untouched and the underlying PDF text remains
/// readable). Content is a centered bold "resume here" label flanked
/// by `─` rules: `─── resume here ───`. Below a narrow-terminal
/// threshold the label is dropped and only the rule renders.
fn draw_resume_marker(f: &mut Frame, app: &mut App<'_>, img_area: Rect) {
    app.expire_resume_marker();
    let Some(marker) = app.resume_marker else { return };
    let cell_h = app.cell_size_px.1.max(1) as i64;
    let viewport_top = app.scroll_y_px;
    let rel_px = marker.doc_y_px - viewport_top;
    if rel_px < 0 || rel_px >= app.viewport_px.1 as i64 {
        return;
    }
    let row = (rel_px / cell_h) as u16;
    if row >= img_area.height {
        return;
    }

    let marker_w = resume_marker_width(img_area.width);
    let line = resume_marker_line(marker_w);
    let strip = Rect {
        x: img_area.x,
        y: img_area.y + row,
        width: marker_w,
        height: 1,
    };
    f.render_widget(Paragraph::new(line), strip);
    // Same trap as the selection overlay: ratatui-image's kitty
    // backend left every cell beyond column 0 with `skip=true`, and
    // Paragraph's render doesn't clear that flag. Clear it here or
    // the marker is buffered but never written to the terminal.
    let buf = f.buffer_mut();
    for x in strip.x..strip.x.saturating_add(strip.width) {
        if let Some(cell) = buf.cell_mut((x, strip.y)) {
            cell.set_skip(false);
        }
    }
}

/// Pure helper: marker width = round(0.3 * image width), clamped to
/// at least 1 cell and at most the image width itself.
pub fn resume_marker_width(image_w: u16) -> u16 {
    let w = ((image_w as f32) * 0.30).round() as u16;
    w.max(1).min(image_w)
}

/// Pure builder for the resume-marker line content. Returns a fully
/// styled `Line` so a `TestBackend` snapshot test can inspect every
/// span (chars + colors + modifiers) without spinning up an `App`.
///
/// Layout:
/// - Below `MIN_LABELED_WIDTH` cells: line-only `────` (yellow).
/// - At or above: `─── resume here ───` with the label centered.
///   Label = bold black on yellow background (pops against any PDF
///   page bitmap regardless of dark mode). Rules = plain yellow.
///   Odd remainder goes to the right flank so the weight feels
///   left-anchored.
///
/// The earlier version used `Modifier::DIM` for both rule and label;
/// some terminals render DIM as ~transparent, which made the label
/// invisible. Solid colours instead.
pub fn resume_marker_line(marker_w: u16) -> Line<'static> {
    const LABEL: &str = "resume here";
    const MIN_LABELED_WIDTH: u16 = (LABEL.len() as u16) + 4 + 2;
    let rule_style = Style::default().fg(Color::Yellow);
    if marker_w < MIN_LABELED_WIDTH {
        let s: String = std::iter::repeat('─').take(marker_w as usize).collect();
        return Line::from(Span::styled(s, rule_style));
    }
    let inner = marker_w - LABEL.len() as u16 - 2;
    let left_rule = inner / 2;
    let right_rule = inner - left_rule;
    let left: String = std::iter::repeat('─').take(left_rule as usize).collect();
    let right: String = std::iter::repeat('─').take(right_rule as usize).collect();
    // Black-on-yellow tag style for the label — guaranteed visible
    // on top of either a light or dark PDF render. BOLD reinforces
    // the focal-point role.
    let label_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled(left, rule_style),
        Span::styled(format!(" {LABEL} "), label_style),
        Span::styled(right, rule_style),
    ])
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

    let fit_width_px = (((viewport_w as f32) * app.zoom).max(1.0) as u32)
        .min(MAX_FIT_WIDTH_PX);
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
    // user's scroll direction. Sent to the background worker (if
    // available) so cold prefetch never blocks the UI.
    let prefetch = app.prefetch_targets(visible.clone());
    for page_idx in prefetch {
        request_prefetch(app, page_idx, fit_width_px);
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
    let canvas = if let Some(c) = try_scroll_shift_canvas(app, &compose_key, viewport_w, viewport_h) {
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

    let fit_width_px =
        (((viewport_w as f32) * app.zoom).max(1.0) as u32).min(MAX_FIT_WIDTH_PX);
    app.ensure_layout(fit_width_px, viewport_h);
    let layout_key = LayoutKey { fit_width_px, dark: app.dark };

    let visible_range = app.layout.visible_pages(app.scroll_y_px, viewport_h);
    let visible: Vec<usize> = visible_range.clone().collect();
    if visible.is_empty() {
        return Ok(());
    }

    {
        let _s = crate::profile::span(crate::profile::Phase::EnsureRendered);
        for &pi in &visible {
            ensure_page_rendered(app, pi, fit_width_px, /*allow_failure=*/ true)?;
            app.touch_page(pi);
        }
    }

    app.evict_far_pages(visible_range.clone());
    app.enforce_byte_budget(page_cache_budget_bytes(), visible_range);

    {
        let _s = crate::profile::span(crate::profile::Phase::EnsureOverlay);
        for &pi in &visible {
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
        width_cells: u16,
    }

    let scroll_y = app.scroll_y_px;
    let area_width_cells = area.width;
    let area_height_cells = area.height;

    let mut blits: Vec<PageBlit> = Vec::with_capacity(visible.len());
    for &page_idx in &visible {
        let Some((bm, _)) = app.overlay_cache.get(&page_idx) else {
            continue;
        };
        let pixel_w = bm.width();
        let pixel_h = bm.height();

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

        // Center horizontally if the rendered page is narrower than
        // the viewport (typical for portrait pages with default zoom).
        let dst_left_cell = if img_w_cells < area_width_cells {
            (area_width_cells - img_w_cells) / 2
        } else {
            0
        };
        let width_cells = img_w_cells.min(area_width_cells.saturating_sub(dst_left_cell));
        if width_cells == 0 {
            continue;
        }

        let revision = compute_page_revision(app, page_idx);
        let kp = app.kitty_pages.as_ref().expect("kitty_pages should be Some on this draw path");
        let need_transmit = !kp.is_fresh(page_idx, layout_key, revision, pixel_w, pixel_h);
        let image_id = kp.image_id(page_idx);

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
            width_cells,
        });
    }

    // Build transmit strings for pages that need them. Held outside
    // the draw loop so the immutable borrow on overlay_cache doesn't
    // overlap the mutable borrow on kitty_pages.
    let transmits: Vec<Option<String>> = {
        let kp = app.kitty_pages.as_ref().unwrap();
        blits
            .iter()
            .map(|b| {
                if !b.need_transmit {
                    return None;
                }
                let bm = app.overlay_cache.get(&b.page_idx).map(|(b, _)| b)?;
                Some(kp.build_transmit(bm, b.page_idx))
            })
            .collect()
    };

    drop(_compose);

    // Emit placements (and prefix the first cell with the transmit
    // string for any page that needed one). The Draw span on the
    // outside of `term.draw` covers the actual write to the pty.
    let buf = f.buffer_mut();
    let img_area_left = area.left();
    let img_area_top = area.top();

    for (b, t) in blits.iter().zip(transmits.iter()) {
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
            b.width_cells,
            t.as_deref(),
        );
    }

    // Now that the buffer has the placements, mark each page as
    // transmitted so the next frame skips the transmit if state is
    // unchanged.
    let kp = app.kitty_pages.as_mut().unwrap();
    for b in &blits {
        if b.need_transmit {
            kp.mark_transmitted(b.page_idx, layout_key, b.revision, b.pixel_w, b.pixel_h);
        }
    }

    Ok(())
}

/// Per-page revision: hash of every overlay-affecting field except
/// the layout (which is tracked separately by the kitty registry).
/// Bumping this for a page invalidates only that page's transmit
/// cache, leaving every other transmitted page in the terminal alone.
fn compute_page_revision(app: &App<'_>, page_idx: usize) -> u64 {
    let (search_revision, has_search_hits, current_hit_on_this_page) = match &app.search {
        Some(s) => {
            let any = s.hits.iter().any(|h| h.page == page_idx);
            let cur = s.current_hit().map(|h| h.page == page_idx).unwrap_or(false);
            (s.revision, any, cur)
        }
        None => (0u64, false, false),
    };
    let sel = app.selection_signature_for_page(page_idx);

    // FNV-1a-style mix; doesn't need to be cryptographic — just
    // non-degenerate enough that flipping any single field flips the
    // revision.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in [
        app.highlight_revision,
        search_revision,
        sel,
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

    let bg = if app.dark {
        Rgba([20, 20, 20, 255])
    } else {
        Rgba([240, 240, 240, 255])
    };
    let bg_row: Vec<u8> = (0..viewport_w).flat_map(|_| bg.0).collect();
    let buf = canvas.as_mut();
    for y in strip_y0..strip_y1 {
        let off = (y as usize) * stride;
        buf[off..off + stride].copy_from_slice(&bg_row);
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
        let Some((page_img, _)) = app.overlay_cache.get(&page_idx) else {
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
    let visible: Vec<usize> = app
        .layout
        .visible_pages(app.scroll_y_px, viewport_h)
        .collect();
    let page_x_origin: i64 = if fit_width_px <= viewport_w {
        ((viewport_w - fit_width_px) / 2) as i64
    } else {
        -(((fit_width_px - viewport_w) as f32) * app.scroll_x).round() as i64
    };

    // Re-blit only the pages whose per-page selection_sig changed
    // since the last compose. We don't have the previous per-page
    // sig captured, so use the cheaper proxy: any page that the
    // selection touches now OR touched on the previous compose. In
    // practice that's at most one or two pages per Visual-mode
    // motion. The pages we re-blit overwrite their existing rows
    // exactly; pages that didn't change keep their pixels.
    let touches_now = pages_touched_by_selection(app);
    for page_idx in &visible {
        if !touches_now.contains(page_idx) {
            continue;
        }
        let Some((page_img, _)) = app.overlay_cache.get(page_idx) else {
            continue;
        };
        let page_doc_y = app.layout.page_y(*page_idx);
        let page_y_in_viewport = page_doc_y - app.scroll_y_px;
        blit_clipped(&mut canvas, page_x_origin, page_y_in_viewport, page_img);
    }

    Some(canvas)
}

/// Set of page indices the active selection currently spans. Empty
/// when there is no selection.
fn pages_touched_by_selection(app: &App<'_>) -> std::collections::HashSet<usize> {
    let mut out = std::collections::HashSet::new();
    if let Some(sel) = app.text_selection {
        let (lo, hi) = sel.ordered();
        for p in lo.page..=hi.page {
            out.insert(p);
        }
    }
    out
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

/// FNV-1a 64-bit over the raw RGBA bytes. Cheap (~1 GB/s on a single
/// core), good enough as an "are these pixels the same" check —
/// collision risk on an 8 MB buffer is negligible for our purposes
/// (a false equality at ~2^-64 is acceptable; the worst outcome is
/// one missing frame). FNV beats DefaultHasher's siphash here by
/// ~5× because we don't need crypto-strength.
fn fnv1a_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
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
        app.pages_in_flight.remove(&(
            resp.req.page,
            resp.req.target_width_px,
            resp.req.dark,
        ));
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
fn ensure_page_rendered(
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
    match pdf::render_page_at_width(&app.document, page_idx, fit_width_px) {
        Ok(img) => {
            let img = if app.dark {
                DynamicImage::ImageRgba8(dark::invert_luminance(&img))
            } else {
                img
            };
            app.page_cache.insert(page_idx, img);
            app.last_compose_key = None;
            Ok(())
        }
        Err(e) => {
            // Mark the page so we don't keep re-attempting; surface
            // a one-shot status line so the user knows.
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

fn page_overlay_key(app: &App<'_>, page_idx: usize, layout: LayoutKey) -> PageOverlayKey {
    let (search_revision, has_search_hits, current_hit_on_this_page) = match &app.search {
        Some(s) => {
            let any = s.hits.iter().any(|h| h.page == page_idx);
            let cur = s
                .current_hit()
                .map(|h| h.page == page_idx)
                .unwrap_or(false);
            (s.revision, any, cur)
        }
        None => (0, false, false),
    };
    PageOverlayKey {
        layout,
        highlight_revision: app.highlight_revision,
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
fn ensure_overlay(app: &mut App<'_>, page_idx: usize, layout: LayoutKey) {
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
    let Some((baked, _)) = app.highlights_baked_cache.get(&page_idx) else {
        return;
    };
    let mut img = baked.clone();

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
            let any = s.hits.iter().any(|h| h.page == page_idx);
            let cur = s.current_hit().map(|h| h.page == page_idx).unwrap_or(false);
            (s.revision, any, cur)
        }
        None => (0, false, false),
    };
    let key = HighlightsBakedKey {
        layout,
        highlight_revision: app.highlight_revision,
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
    let mut img = src.to_rgba8();

    for h in app.highlights.for_page(page_idx) {
        let rect = norm_to_pixels(
            Rect01 { x: h.x, y: h.y, w: h.w, h: h.h },
            img.width(),
            img.height(),
        );
        let rgb = rgb_from_hex(&h.color);
        fill_rect_blend(&mut img, rect, rgb, 0.35);
    }

    if let Some(s) = &app.search {
        let current_idx = s.current;
        for (i, hit) in s.hits.iter().enumerate().filter(|(_, h)| h.page == page_idx) {
            let rect = norm_to_pixels(hit.rect, img.width(), img.height());
            let color = (255u8, 165, 0);
            fill_rect_blend(&mut img, rect, color, 0.45);
            if i == current_idx {
                outline_rect(&mut img, rect, (255, 80, 0), 3);
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
    let visible = app.layout.visible_pages(app.scroll_y_px, viewport_h);

    let page_x_origin: i64 = if fit_width_px <= viewport_w {
        ((viewport_w - fit_width_px) / 2) as i64
    } else {
        -(((fit_width_px - viewport_w) as f32) * app.scroll_x).round() as i64
    };

    // Build a list of (start_row, end_row) extents that the visible
    // pages cover in the viewport. Then fill the inverse with bg.
    let mut covered: Vec<(i64, i64)> = Vec::with_capacity(8);
    for page_idx in visible.clone() {
        let page_doc_y = app.layout.page_y(page_idx);
        let page_h = app.layout.page_h(page_idx) as i64;
        let top = (page_doc_y - app.scroll_y_px).max(0);
        let bot = (page_doc_y - app.scroll_y_px + page_h).min(viewport_h as i64);
        if bot > top { covered.push((top, bot)); }
    }

    fill_gap_rows(&mut canvas, viewport_w, viewport_h, &covered, bg);

    // If the page is narrower than the viewport (zoomed-out or
    // portrait page), the side margins between (0..page_x_origin)
    // and (page_x_origin+fit_width_px..viewport_w) also need bg.
    if fit_width_px < viewport_w {
        fill_side_margins(&mut canvas, viewport_w, viewport_h, page_x_origin, fit_width_px, bg);
    }

    for page_idx in visible {
        let Some((page_img, _)) = app.overlay_cache.get(&page_idx) else {
            continue;
        };
        let page_doc_y = app.layout.page_y(page_idx);
        let page_y_in_viewport = page_doc_y - app.scroll_y_px;
        blit_clipped(&mut canvas, page_x_origin, page_y_in_viewport, page_img);
    }

    canvas
}

/// Paint `bg` into every row of `canvas` that no entry in `covered`
/// includes. Each `covered` entry is (top, bot) in viewport rows.
/// We don't need overlap merging — we just iterate ALL rows, mark
/// which are covered, and fill the rest. Cheap because we touch
/// pixels at most once per row.
fn fill_gap_rows(
    canvas: &mut RgbaImage,
    viewport_w: u32,
    viewport_h: u32,
    covered: &[(i64, i64)],
    bg: Rgba<u8>,
) {
    let row_bytes = (viewport_w as usize) * 4;
    // Scratch row of the bg color we can copy_from_slice into target rows.
    // Allocated lazily — only built once even across a long session because
    // bg is a constant.
    let bg_row: Vec<u8> = (0..viewport_w)
        .flat_map(|_| bg.0)
        .collect();
    let buf = canvas.as_mut();
    'rows: for y in 0..viewport_h as i64 {
        for &(top, bot) in covered {
            if y >= top && y < bot {
                continue 'rows;
            }
        }
        let off = (y as usize) * row_bytes;
        buf[off..off + row_bytes].copy_from_slice(&bg_row);
    }
}

/// Paint side margins (cells outside the page's horizontal extent)
/// with `bg`. Only called when the page is narrower than the viewport.
fn fill_side_margins(
    canvas: &mut RgbaImage,
    viewport_w: u32,
    viewport_h: u32,
    page_x_origin: i64,
    fit_width_px: u32,
    bg: Rgba<u8>,
) {
    let left_end = page_x_origin.max(0).min(viewport_w as i64) as u32;
    let right_start = (page_x_origin + fit_width_px as i64).max(0).min(viewport_w as i64) as u32;
    let buf = canvas.as_mut();
    let row_bytes = (viewport_w as usize) * 4;
    for y in 0..viewport_h as usize {
        let row = &mut buf[y * row_bytes..(y + 1) * row_bytes];
        for x in 0..left_end as usize {
            row[x * 4..x * 4 + 4].copy_from_slice(&bg.0);
        }
        for x in right_start as usize..viewport_w as usize {
            row[x * 4..x * 4 + 4].copy_from_slice(&bg.0);
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
                if let Some(s) = pt.line_start(line) { start = s; }
            }
            if let Some(line) = pt.line_of(end) {
                if let Some(e) = pt.line_end(line) { end = e; }
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
            if caret_rect.2 < 2 { caret_rect.2 = 2; }
            if caret_rect.3 < 2 { caret_rect.3 = 2; }
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

fn status_line(app: &App<'_>) -> Paragraph<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();

    let mode_label = match app.mode {
        Mode::Normal => "",
        Mode::Visual => " VISUAL ",
        Mode::Command => ":",
        Mode::Search => "/",
    };

    if !mode_label.is_empty() {
        spans.push(Span::styled(
            mode_label.to_string(),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if matches!(app.mode, Mode::Command | Mode::Search) {
        spans.push(Span::raw(app.cmd_buffer.clone()));
    } else {
        spans.push(Span::styled(
            format!(" {}/{}  ", app.current_page() + 1, app.page_count),
            Style::default().fg(Color::White),
        ));
        if app.dark {
            spans.push(Span::styled(
                "DARK  ".to_string(),
                Style::default().fg(Color::Cyan),
            ));
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
            "    ?  for help".to_string(),
            Style::default().fg(Color::DarkGray),
        ));
    }

    Paragraph::new(Line::from(spans))
}

fn draw_toc(f: &mut Frame, app: &App<'_>, area: Rect) {
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
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title.bold()));
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
        "                         (Space drops a 3s 'resume here' line at your last bottom)",
        "  Ctrl-d / Ctrl-u        scroll a half-screen down / up",
        "  gg / G                 doc top / bottom",
        "  N G                    jump to page N",
        "",
        "  arrows / h / l         scroll in pixel-sized steps",
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
        "  o  /  :toc             open outline panel",
        "    j/k Enter            navigate / jump to entry",
        "    / type Enter         filter by substring",
        "    Esc                  close panel",
        "  :<n>  /  :goto N       jump to page n",
        "  :export [path]         dump highlights as Markdown notes",
        "  :q                     quit",
        "  :set dark | :set nodark",
        "  ?                      toggle this overlay",
        "  q                      quit",
        "",
        "Press ? or Esc to close",
    ]
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
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;
    use crate::textlayout::{Caret, CharCell, LineSpan, PageText, SelMode, TextSelection};

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
        assert_eq!(buf[0 * stride], 1, "row 0 should hold ex-row-1");
        assert_eq!(buf[1 * stride], 2, "row 1 should hold ex-row-2");
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
        assert_eq!(buf, snapshot, "dy >= viewport_h must leave buffer untouched");

        shift_canvas_rows(&mut buf, w, h, -4);
        assert_eq!(buf, snapshot, "|dy| >= viewport_h must leave buffer untouched");
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
                    bbox: Rect01 { x, y: y_top, w: 0.08, h: y_bot - y_top },
                    line,
                    origin_x: x,
                    is_generated: false,
                });
            }
        }
        let lines = vec![
            LineSpan { y_top: 0.10, y_bot: 0.20, start_idx: 0, end_idx: 4, word_starts: vec![0] },
            LineSpan { y_top: 0.40, y_bot: 0.50, start_idx: 5, end_idx: 9, word_starts: vec![5] },
        ];
        PageText { page_idx: 0, chars, lines, width_pts: 100.0, height_pts: 100.0 }
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
        let sel = TextSelection { anchor: caret, head: caret, mode: SelMode::Charwise };

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
                if p[0] != 255 || p[1] != 255 || p[2] != 255 { non_white += 1; }
            }
        }
        assert!(
            non_white >= 4,
            "placement mode must still draw the caret accent inside the head's bbox \
             (found {non_white} non-white pixels)"
        );
    }

    // ---- Resume-marker pure builder ---------------------------------

    #[test]
    fn resume_marker_width_is_30_percent() {
        assert_eq!(resume_marker_width(100), 30);
        assert_eq!(resume_marker_width(80), 24);
        assert_eq!(resume_marker_width(50), 15);
        // Tiny terminals: clamped to ≥ 1 cell.
        assert_eq!(resume_marker_width(1), 1);
        assert_eq!(resume_marker_width(2), 1);
    }

    #[test]
    fn resume_marker_line_full_width_has_label() {
        let line = resume_marker_line(40);
        // 3 spans expected: left rule, " resume here ", right rule.
        let spans: Vec<&Span<'_>> = line.spans.iter().collect();
        assert_eq!(spans.len(), 3, "expected 3 spans, got {:?}", spans);
        assert!(spans[0].content.chars().all(|c| c == '─'));
        assert_eq!(spans[1].content, " resume here ");
        assert!(spans[2].content.chars().all(|c| c == '─'));
        // Total width must equal what we asked for.
        let total: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        assert_eq!(total, 40);
    }

    #[test]
    fn resume_marker_line_label_is_bold_black_on_yellow() {
        // Black-on-yellow tag style — visible on any PDF page bitmap
        // regardless of dark mode. Earlier `Modifier::DIM` rendered as
        // ~transparent on some terminals, hiding the label entirely.
        let line = resume_marker_line(40);
        let label_span = &line.spans[1];
        assert_eq!(label_span.style.fg, Some(Color::Black));
        assert_eq!(label_span.style.bg, Some(Color::Yellow));
        assert!(label_span.style.add_modifier.contains(Modifier::BOLD));
        assert!(
            !label_span.style.add_modifier.contains(Modifier::DIM),
            "DIM is unreliable across terminals; never set it on the label"
        );
    }

    #[test]
    fn resume_marker_line_rules_are_solid_yellow() {
        let line = resume_marker_line(40);
        for (i, span) in [&line.spans[0], &line.spans[2]].iter().enumerate() {
            assert_eq!(span.style.fg, Some(Color::Yellow), "rule span {i}");
            assert!(
                !span.style.add_modifier.contains(Modifier::DIM),
                "rule span {i} must not be DIM (terminal-dependent)"
            );
        }
    }

    #[test]
    fn resume_marker_line_narrow_falls_back_to_line_only() {
        let line = resume_marker_line(10);
        assert_eq!(line.spans.len(), 1, "narrow fallback = single span");
        assert!(line.spans[0].content.chars().all(|c| c == '─'));
        assert_eq!(line.spans[0].content.chars().count(), 10);
        assert_eq!(line.spans[0].style.fg, Some(Color::Yellow));
    }

    /// Render the marker through a real (test) backend and walk the
    /// resulting cell buffer. The closest thing to "did the terminal
    /// get the right bytes" that we can write headlessly.
    #[test]
    fn resume_marker_renders_to_buffer_with_correct_chars_and_colors() {
        let backend = TestBackend::new(40, 1);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            let line = resume_marker_line(40);
            f.render_widget(
                Paragraph::new(line),
                Rect { x: 0, y: 0, width: 40, height: 1 },
            );
        })
        .unwrap();
        let buf = term.backend().buffer();
        let mut rendered = String::new();
        let mut yellow_bg_cells = 0;
        let mut yellow_fg_cells = 0;
        let mut bold_cells = 0;
        for x in 0..40 {
            let cell = buf.cell((x, 0)).unwrap();
            rendered.push_str(cell.symbol());
            if cell.bg == Color::Yellow {
                yellow_bg_cells += 1;
            }
            if cell.fg == Color::Yellow {
                yellow_fg_cells += 1;
            }
            if cell.modifier.contains(Modifier::BOLD) {
                bold_cells += 1;
            }
        }
        assert!(
            rendered.contains("resume here"),
            "rendered cells must contain the label, got {rendered:?}"
        );
        assert!(
            rendered.starts_with('─') && rendered.ends_with('─'),
            "rendered cells should start and end with rule glyphs, got {rendered:?}"
        );
        // Label is the 13-cell tag. Black fg on yellow BG, bold.
        assert_eq!(
            yellow_bg_cells, 13,
            "exactly the 13-cell label should have yellow background"
        );
        assert_eq!(bold_cells, 13, "exactly the label should be bold");
        // Rules are the other 27 cells: yellow FG, no bg.
        assert_eq!(
            yellow_fg_cells, 27,
            "rule cells must be yellow fg (27 = 40 - 13)"
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
    fn help_overlay_mentions_resume_marker() {
        let body = help_overlay_lines().join("\n");
        assert!(
            body.contains("resume here"),
            "Space-marker feature should be documented in the help overlay"
        );
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
                Rect { x: 0, y: 0, width: 50, height: 20 },
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
}
