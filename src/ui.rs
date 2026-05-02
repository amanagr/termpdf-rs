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
use ratatui_image::{Resize, StatefulImage};

use crate::app::{App, ComposeKey, LayoutKey, Mode};
use crate::compose::{fill_rect_blend, norm_to_pixels, outline_rect};
use crate::dark;
use crate::highlight::{rgb_from_hex, Rect01, HIGHLIGHT_COLORS};
use crate::pdf;

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
        // centered/cropped while painting), so we just hand it to
        // ratatui-image at the image area unchanged.
        let fit = Resize::Fit(None);
        let render_size = proto.size_for(fit, img_area);
        let placed = Rect {
            x: img_area.x,
            y: img_area.y,
            width: render_size.width.min(img_area.width),
            height: img_area.height,
        };
        f.render_stateful_widget(StatefulImage::<StatefulProtocol>::new(), placed, proto);
    }

    f.render_widget(status_line(app), status_area);

    if app.show_help {
        draw_help(f, area);
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

    let fit_width_px = ((viewport_w as f32) * app.zoom).max(1.0) as u32;
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
        if !app.page_cache.contains_key(&page_idx) {
            let img = pdf::render_page_at_width(&app.document, page_idx, fit_width_px)?;
            let img = if app.dark {
                DynamicImage::ImageRgba8(dark::invert_luminance(&img))
            } else {
                img
            };
            app.page_cache.insert(page_idx, img);
            app.last_compose_key = None; // force compose to repaint
        }
    }
    app.evict_far_pages(visible.clone());

    // Compose key: changes to scroll, selection, highlight count, or
    // viewport invalidate the cached canvas.
    let sel_key = app.selection.map(|s| {
        (
            app.selection_page,
            (s.x * 10000.0) as u32,
            (s.y * 10000.0) as u32,
            (s.w * 10000.0) as u32,
            (s.h * 10000.0) as u32,
        )
    });
    let compose_key = ComposeKey {
        layout: layout_key,
        viewport_w,
        viewport_h,
        scroll_y_px: app.scroll_y_px,
        scroll_x_milli: (app.scroll_x * 10000.0) as u32,
        selection: sel_key,
        selection_color_idx: app.selection_color_idx,
        highlight_revision: app.highlight_revision,
    };
    if app.last_compose_key == Some(compose_key) && app.image_proto.is_some() {
        return Ok(());
    }

    let canvas = compose_viewport(app, viewport_w, viewport_h);
    app.image_proto = Some(app.picker.new_resize_protocol(DynamicImage::ImageRgba8(canvas)));
    app.last_compose_key = Some(compose_key);
    Ok(())
}

/// Paint the viewport canvas: stitch slices of every visible page
/// into a single RgbaImage, applying per-page highlight overlays
/// and the active Visual-mode selection.
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

    // Horizontal positioning of the page strip in the viewport.
    let page_x_origin: i64 = if fit_width_px <= viewport_w {
        ((viewport_w - fit_width_px) / 2) as i64
    } else {
        -(((fit_width_px - viewport_w) as f32) * app.scroll_x).round() as i64
    };

    for page_idx in visible {
        let Some(page_img) = app.page_cache.get(&page_idx) else {
            continue;
        };
        let mut page_rgba = page_img.to_rgba8();

        // Saved highlights for this page → translucent fill, no border.
        for h in app.highlights.for_page(page_idx) {
            let rect = norm_to_pixels(
                Rect01 { x: h.x, y: h.y, w: h.w, h: h.h },
                page_rgba.width(),
                page_rgba.height(),
            );
            let rgb = rgb_from_hex(&h.color);
            fill_rect_blend(&mut page_rgba, rect, rgb, 0.35);
        }
        // Active Visual-mode selection if it lives on this page.
        if let Some(sel) = app.selection {
            if app.selection_page == page_idx {
                let rect = norm_to_pixels(sel, page_rgba.width(), page_rgba.height());
                let color = HIGHLIGHT_COLORS[app.selection_color_idx % HIGHLIGHT_COLORS.len()];
                fill_rect_blend(&mut page_rgba, rect, color.rgb, 0.30);
                outline_rect(&mut page_rgba, rect, color.rgb, 2);
            }
        }

        // Position of this page within the viewport.
        let page_doc_y = app.layout.page_y(page_idx);
        let page_y_in_viewport = page_doc_y - app.scroll_y_px;
        blit_clipped(
            &mut canvas,
            page_x_origin,
            page_y_in_viewport,
            &page_rgba,
        );
    }

    canvas
}

/// Blit `src` onto `dst` at position `(dst_x, dst_y)`, clipping to
/// the destination's bounds. Coordinates are signed so a partially
/// off-screen src (top of first visible page above the viewport,
/// for example) just gets clipped instead of panicking.
fn blit_clipped(dst: &mut RgbaImage, dst_x: i64, dst_y: i64, src: &RgbaImage) {
    let dw = dst.width() as i64;
    let dh = dst.height() as i64;
    let sw = src.width() as i64;
    let sh = src.height() as i64;

    // Source rect: figure out which portion of `src` lands inside dst.
    let sx0 = (-dst_x).max(0);
    let sy0 = (-dst_y).max(0);
    let sx1 = (dw - dst_x).min(sw);
    let sy1 = (dh - dst_y).min(sh);
    if sx1 <= sx0 || sy1 <= sy0 {
        return;
    }

    for sy in sy0..sy1 {
        for sx in sx0..sx1 {
            let sp = src.get_pixel(sx as u32, sy as u32);
            let dx = (dst_x + sx) as u32;
            let dy = (dst_y + sy) as u32;
            dst.put_pixel(dx, dy, *sp);
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

fn draw_help(f: &mut Frame, area: Rect) {
    let help_lines: Vec<&str> = vec![
        "termpdf-rs — continuous-scroll PDF reader",
        "",
        "  j / k                  next / prev page (jump to page boundary)",
        "  N j  /  N k            jump N pages forward / back",
        "  Space / Ctrl-d         scroll a screen / half a screen down",
        "  b / Ctrl-u             scroll up: page boundary / half screen",
        "  gg / G                 doc top / bottom",
        "  N G                    jump to page N",
        "",
        "  arrows / h / l         scroll in pixel-sized steps",
        "  mouse wheel            scroll (Shift = horizontal)",
        "",
        "  + / - / 0              zoom in / out / reset",
        "  d                      toggle dark mode (luminance-only)",
        "",
        "  v                      enter Visual mode (keyboard highlight)",
        "    hjkl / arrows        move selection",
        "    HJKL / Shift+arrows  resize selection",
        "    c                    cycle highlight color",
        "    y / Enter            save highlight",
        "    Esc                  cancel",
        "  click + drag           highlight with the mouse",
        "  x                      delete last highlight on current page",
        "",
        "  /<query>               search (stub)",
        "  :<n>  /  :goto N       jump to page n",
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
