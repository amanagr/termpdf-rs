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
use crate::textlayout::SelMode;

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

    if let Err(e) = ensure_image(app, img_area) {
        f.render_widget(
            Paragraph::new(format!("render error: {e:#}"))
                .style(Style::default().fg(Color::Red)),
            img_area,
        );
    } else if let Some(proto) = app.image_proto.as_mut() {
        // The composed image is exactly viewport-sized (we already
        // centered/cropped while painting), so render it across the
        // full image area. Previously we trimmed the placed width to
        // `render_size.width.min(img_area.width)`, which left a strip
        // of cells on the right with no kitty placeholders — the
        // terminal painted those as default-bg black, most visibly
        // when the help popup was open and the user could see them.
        f.render_stateful_widget(
            StatefulImage::<StatefulProtocol>::new(),
            img_area,
            proto,
        );
    }

    // Selection overlay paints AFTER the kitty image so it overwrites
    // the image-fragment placeholder cells with our colored blocks.
    // Crucially, the selection lives entirely in the cell layer — no
    // bitmap rebuild, no kitty re-encode, no terminal-side image churn.
    draw_selection_overlay(f, app, img_area);

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
    // The active Visual-mode selection is NOT baked here. It paints
    // over the kitty image as a cell overlay in `draw_selection_overlay`,
    // so motion is independent of the page bitmap and the kitty re-encode.

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

/// Paint the active Visual-mode selection as a translucent block over
/// the kitty image area. Snaps to terminal cells (the only granular
/// unit ratatui can paint), which is intentional: pixel-precise
/// selection feedback would require re-rendering and re-uploading the
/// kitty image on every keystroke — the very thing that crashed
/// Ghostty in the previous design.
///
/// We use a half-shade glyph (`▒`) with the highlight colour as fg
/// rather than a solid bg-coloured space. Solid bg masks the page
/// content under the selection; the shade leaves ~50% of the cell
/// transparent so the user can still read what they're selecting.
fn draw_selection_overlay(f: &mut ratatui::Frame, app: &App<'_>, img_area: Rect) {
    if app.mode != Mode::Visual {
        return;
    }
    let Some(sel) = app.text_selection else {
        return;
    };

    let (cell_w, cell_h) = app.cell_size_px;
    let (vw, vh) = app.viewport_px;
    if cell_w == 0 || cell_h == 0 || vw == 0 || vh == 0 {
        return;
    }

    let color = HIGHLIGHT_COLORS[app.selection_color_idx % HIGHLIGHT_COLORS.len()];
    let (r, g, b) = color.rgb;
    let style = Style::default()
        .fg(Color::Rgb(r, g, b))
        .add_modifier(Modifier::BOLD);

    let (lo, hi) = sel.ordered();
    for page_idx in lo.page..=hi.page {
        let Some(pt) = app.text_cache.get(page_idx) else { continue };
        // Charwise: the selection is the inclusive char range
        //   [start, end] within the page.
        // Linewise: extend to whole lines (start = first char of
        //   start's line, end = last char of end's line).
        // Blockwise: same span but the renderer below intersects per-
        //   line with a column band [min_x, max_x].
        let mut start = if page_idx == lo.page { lo.idx } else { 0 };
        let mut end = if page_idx == hi.page {
            hi.idx
        } else {
            pt.chars.len().saturating_sub(1)
        };
        if matches!(sel.mode, SelMode::Linewise) {
            if let Some(line) = pt.line_of(start) {
                if let Some(s) = pt.line_start(line) { start = s; }
            }
            if let Some(line) = pt.line_of(end) {
                if let Some(e) = pt.line_end(line) { end = e; }
            }
        }

        let rects = if matches!(sel.mode, SelMode::Blockwise) {
            blockwise_rects(pt, lo, hi, page_idx)
        } else {
            pt.range_to_rects(start, end)
        };
        for r01 in rects {
            paint_rect_cells(f, app, img_area, page_idx, r01, style);
        }
    }

    // Caret cursor at the head, painted with a thicker fg so the user
    // knows where motions are anchored.
    if let Some(pt) = app.text_cache.get(sel.head.page) {
        if let Some(cell) = pt.chars.get(sel.head.idx) {
            let caret_style = Style::default()
                .fg(Color::Rgb(255, 255, 255))
                .bg(Color::Rgb(r, g, b))
                .add_modifier(Modifier::BOLD);
            paint_rect_cells(f, app, img_area, sel.head.page, cell.bbox, caret_style);
        }
    }
}

/// Compute per-line rects for visual-block selection: a rectangular
/// range from `min_col_x` to `max_col_x` (in normalised PDF page x)
/// intersected with each visual line covered by the selection.
fn blockwise_rects(
    pt: &crate::textlayout::PageText,
    lo: crate::textlayout::Caret,
    hi: crate::textlayout::Caret,
    page_idx: usize,
) -> Vec<Rect01> {
    let lo_idx = if page_idx == lo.page { lo.idx } else { 0 };
    let hi_idx = if page_idx == hi.page {
        hi.idx
    } else {
        pt.chars.len().saturating_sub(1)
    };
    if pt.chars.is_empty() {
        return Vec::new();
    }
    let (lo_idx, hi_idx) = (lo_idx.min(hi_idx), lo_idx.max(hi_idx));
    let line_lo = pt.chars[lo_idx].line;
    let line_hi = pt.chars[hi_idx].line;
    // Column band defined by the lo and hi carets' x positions.
    let lo_x = pt.chars[lo_idx].bbox.x;
    let hi_x = pt.chars[hi_idx].bbox.x + pt.chars[hi_idx].bbox.w;
    let (xa, xb) = if lo_x <= hi_x { (lo_x, hi_x) } else { (hi_x, lo_x) };

    let mut out = Vec::new();
    for line_idx in line_lo..=line_hi {
        let span = match pt.lines.get(line_idx) {
            Some(s) => s,
            None => continue,
        };
        // Build a rect from chars on this line whose x falls inside [xa, xb].
        let mut rect: Option<Rect01> = None;
        for i in span.start_idx..=span.end_idx {
            let c = &pt.chars[i];
            if c.line != line_idx {
                continue;
            }
            let cx_lo = c.bbox.x;
            let cx_hi = c.bbox.x + c.bbox.w;
            if cx_hi < xa || cx_lo > xb {
                continue;
            }
            rect = Some(match rect {
                Some(prev) => union_rect_pub(prev, c.bbox),
                None => c.bbox,
            });
        }
        if let Some(r) = rect {
            out.push(r);
        }
    }
    out
}

fn union_rect_pub(a: Rect01, b: Rect01) -> Rect01 {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.w).max(b.x + b.w);
    let y1 = (a.y + a.h).max(b.y + b.h);
    Rect01 {
        x: x0,
        y: y0,
        w: (x1 - x0).max(0.0),
        h: (y1 - y0).max(0.0),
    }
}

/// Paint a normalised page rect as terminal cells (half-shade `▒`)
/// over the kitty image area. Shared between `draw_selection_overlay`
/// (any number of per-line rects) and any future caret cursor.
fn paint_rect_cells(
    f: &mut ratatui::Frame,
    app: &App<'_>,
    img_area: Rect,
    page_idx: usize,
    rect: Rect01,
    style: Style,
) {
    let (cell_w, cell_h) = app.cell_size_px;
    let (vw, _) = app.viewport_px;
    let fit_width_px = app.layout.fit_width_px;
    let page_x_origin: i64 = if fit_width_px <= vw {
        ((vw - fit_width_px) / 2) as i64
    } else {
        -(((fit_width_px - vw) as f32) * app.scroll_x).round() as i64
    };

    let page_y = app.layout.page_y(page_idx);
    let page_h = app.layout.page_h(page_idx) as f32;
    let page_top_in_viewport = page_y - app.scroll_y_px;

    let rect_left = page_x_origin + (rect.x * fit_width_px as f32) as i64;
    let rect_top = page_top_in_viewport + (rect.y * page_h) as i64;
    let rect_right = page_x_origin + ((rect.x + rect.w) * fit_width_px as f32) as i64;
    let rect_bot = page_top_in_viewport + ((rect.y + rect.h) * page_h) as i64;

    let cw = cell_w as i64;
    let ch = cell_h as i64;
    let cell_x0 = rect_left.div_euclid(cw);
    let cell_y0 = rect_top.div_euclid(ch);
    let cell_x1 = (rect_right + cw - 1).div_euclid(cw);
    let cell_y1 = (rect_bot + ch - 1).div_euclid(ch);

    let area_x0 = img_area.x as i64;
    let area_y0 = img_area.y as i64;
    let area_x1 = (img_area.x + img_area.width) as i64;
    let area_y1 = (img_area.y + img_area.height) as i64;

    let abs_x0 = (area_x0 + cell_x0).clamp(area_x0, area_x1);
    let abs_y0 = (area_y0 + cell_y0).clamp(area_y0, area_y1);
    let abs_x1 = (area_x0 + cell_x1).clamp(area_x0, area_x1);
    let abs_y1 = (area_y0 + cell_y1).clamp(area_y0, area_y1);
    if abs_x1 <= abs_x0 || abs_y1 <= abs_y0 {
        return;
    }

    let buf = f.buffer_mut();
    for y in abs_y0..abs_y1 {
        for x in abs_x0..abs_x1 {
            if let Some(cell) = buf.cell_mut((x as u16, y as u16)) {
                cell.set_char('▒');
                cell.set_style(style);
            }
        }
    }
}

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
    let help_lines: Vec<&str> = vec![
        "termpdf-rs — continuous-scroll PDF reader",
        "",
        "  j / k                  next / prev page (jump to page boundary)",
        "  N j  /  N k            jump N pages forward / back",
        "  Space / b              scroll one screen down / up (less-style)",
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
        "  x                      delete last highlight on current page",
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
    ];

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
