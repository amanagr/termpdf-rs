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

use crate::app::{App, ComposeKey, LayoutKey, Mode, PageOverlayKey};
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
    } else if let Err(e) = ensure_image(app, img_area) {
        f.render_widget(
            Paragraph::new(format!("render error: {e:#}"))
                .style(Style::default().fg(Color::Red)),
            img_area,
        );
    } else if let Some(proto) = app.image_proto.as_mut() {
        // The composed image is exactly viewport-sized (we already
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
    // LayoutKey because `ensure_layout` clears the cache on change.
    let visible = app.layout.visible_pages(app.scroll_y_px, viewport_h);
    for page_idx in visible.clone() {
        ensure_page_rendered(app, page_idx, fit_width_px, /*allow_failure=*/ true)?;
        app.touch_page(page_idx);
    }

    // Speculatively render a few pages outside the viewport in the
    // user's scroll direction. Failures here are swallowed —
    // prefetch is best-effort, the user hasn't asked to see these
    // pages yet.
    let prefetch = app.prefetch_targets(visible.clone());
    for page_idx in prefetch {
        let _ = ensure_page_rendered(app, page_idx, fit_width_px, true);
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
    for page_idx in visible {
        ensure_overlay(app, page_idx, layout_key);
    }

    let canvas = compose_viewport(app, viewport_w, viewport_h);
    app.image_proto = Some(app.build_protocol(DynamicImage::ImageRgba8(canvas)));
    app.last_compose_key = Some(compose_key);
    Ok(())
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

/// Build (or refresh) the cached overlay bitmap for `page_idx`. The
/// overlay bitmap is the raw pdfium output with saved highlights
/// for this page alpha-blended in, plus the Visual-mode selection
/// if it currently lives on this page. During a mouse-drag this
/// runs only for the page under the cursor — every other visible
/// page reuses its already-overlaid bitmap.
fn ensure_overlay(app: &mut App<'_>, page_idx: usize, layout: LayoutKey) {
    let key = page_overlay_key(app, page_idx, layout);
    if app
        .overlay_cache
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

    // Saved highlights for this page → translucent fill, no border.
    for h in app.highlights.for_page(page_idx) {
        let rect = norm_to_pixels(
            Rect01 { x: h.x, y: h.y, w: h.w, h: h.h },
            img.width(),
            img.height(),
        );
        let rgb = rgb_from_hex(&h.color);
        fill_rect_blend(&mut img, rect, rgb, 0.35);
    }
    // Active Visual-mode selection (if it lives on this page) → bake
    // a slightly stronger translucent fill at the same position the
    // saved highlight would land. This goes through the kitty re-upload
    // path that already works in tmux+Ghostty, instead of trying to
    // overwrite placeholder cells (which the renderer skips because
    // ratatui-image packs each row's escape into col 0 and uses the
    // remaining cells' width attribute to advance the cursor — a
    // post-image cell write never reaches the wire).
    if let Some(sel) = app.text_selection {
        if let Some(pt) = app.text_cache.get(page_idx) {
            bake_selection_into_page(
                &mut img,
                page_idx,
                sel,
                pt,
                app.selection_color_idx,
            );
        }
    }

    // Search hits on this page → orange translucent fill; the
    // current hit additionally gets a thicker outline so the user
    // can see *which* match `n`/`N` is on without reading the count.
    if let Some(s) = &app.search {
        let current_idx = s.current;
        for (i, hit) in s.hits.iter().enumerate().filter(|(_, h)| h.page == page_idx) {
            let rect = norm_to_pixels(hit.rect, img.width(), img.height());
            let color = (255u8, 165, 0); // orange
            fill_rect_blend(&mut img, rect, color, 0.45);
            if i == current_idx {
                outline_rect(&mut img, rect, (255, 80, 0), 3);
            }
        }
    }

    app.overlay_cache.insert(page_idx, (img, key));
}

/// Paint the viewport canvas: stitch the cached overlay bitmap of
/// every visible page into a single RgbaImage. No per-page
/// allocation here — `ensure_overlay` did that work above.
fn compose_viewport(app: &App<'_>, viewport_w: u32, viewport_h: u32) -> RgbaImage {
    // Background colour matches the page background so the inter-
    // page gap looks intentional rather than like a render error.
    let bg = if app.dark { Rgba([20, 20, 20, 255]) } else { Rgba([240, 240, 240, 255]) };
    let mut canvas = RgbaImage::from_pixel(viewport_w, viewport_h, bg);

    let fit_width_px = app.layout.fit_width_px;
    let visible = app.layout.visible_pages(app.scroll_y_px, viewport_h);
    if visible.is_empty() {
        return canvas;
    }

    let page_x_origin: i64 = if fit_width_px <= viewport_w {
        ((viewport_w - fit_width_px) / 2) as i64
    } else {
        -(((fit_width_px - viewport_w) as f32) * app.scroll_x).round() as i64
    };

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
) {
    let (lo, hi) = sel.ordered();
    if !(lo.page..=hi.page).contains(&page_idx) {
        return;
    }
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
        "  v                      enter Visual mode (text caret)",
        "    h j k l              move caret by char / line",
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

        bake_selection_into_page(&mut img, 0, sel, &pt, 0 /* yellow */);

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

        bake_selection_into_page(&mut img, 0, sel, &pt, 0);

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
        bake_selection_into_page(&mut img, 0, sel, &pt, 0);
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
