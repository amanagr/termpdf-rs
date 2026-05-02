//! ratatui frame composition: page image + 1-row status line + ?
//! help overlay. The image itself is rendered by ratatui-image, which
//! handles Kitty/Sixel chunking transparently.
//!
//! Render pipeline (called per frame, but each layer is cached):
//!   1. **pdfium** — rasterize the page at viewport_px × zoom. Cached
//!      by (page, dark, area, zoom); slow path, ~tens of ms.
//!   2. **compose** — clone the cached pixmap, alpha-blend saved
//!      highlights for this page, alpha-blend the Visual-mode
//!      selection if present, then crop to viewport at the current
//!      scroll offset. Cached by everything in step 1 plus scroll +
//!      selection + highlight count.
//!   3. **submit** — wrap the composed image in a `StatefulProtocol`
//!      and hand it to ratatui-image for kitty-graphics encoding.

use anyhow::Result;
use image::{DynamicImage, RgbaImage};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};

use crate::app::{App, ComposeKey, Mode, PdfiumKey};
use crate::compose::{crop_rect, fill_rect_blend, norm_to_pixels, outline_rect};
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

    if let Err(e) = ensure_image(app, img_area) {
        f.render_widget(
            Paragraph::new(format!("render error: {e:#}"))
                .style(Style::default().fg(Color::Red)),
            img_area,
        );
    } else if let Some(proto) = app.image_proto.as_mut() {
        // Horizontal centering. The composed image is at most
        // viewport_w_px wide; for portrait pages at zoom=1 it's
        // narrower (aspect-ratio preserved). Center the cell rect
        // so the page sits in the middle of the terminal instead of
        // pinned to the left edge.
        // Turbofish: StatefulImage<T> is generic over the protocol type
        // and there's nothing in this call that constrains T, so the
        // compiler can't infer it from `proto`. Pin it to StatefulProtocol.
        let fit = Resize::Fit(None);
        let render_size = proto.size_for(fit.clone(), img_area);
        let x_offset = img_area.width.saturating_sub(render_size.width) / 2;
        let centered = Rect {
            x: img_area.x + x_offset,
            y: img_area.y,
            width: render_size.width.min(img_area.width),
            height: img_area.height,
        };
        f.render_stateful_widget(StatefulImage::<StatefulProtocol>::new(), centered, proto);
    }

    f.render_widget(status_line(app), status_area);

    if app.show_help {
        draw_help(f, area);
    }
}

fn ensure_image(app: &mut App<'_>, area: Rect) -> Result<()> {
    let pdfium_key = PdfiumKey {
        page: app.page,
        dark: app.dark,
        area_w: area.width,
        area_h: area.height,
        zoom_milli: (app.zoom * 1000.0) as u32,
    };

    // Step 1: pdfium rasterize (cached).
    if app.last_pdfium_key != Some(pdfium_key) || app.rendered_page.is_none() {
        let img = pdf::render_page(&app.document, app.page, area, &app.picker, app.zoom)?;
        let img = if app.dark {
            DynamicImage::ImageRgba8(dark::invert_luminance(&img))
        } else {
            img
        };
        app.full_px = (img.width(), img.height());
        app.rendered_page = Some(img);
        app.last_pdfium_key = Some(pdfium_key);
        // pdfium output changed → composed cache is stale.
        app.last_compose_key = None;
    }

    // Viewport pixel size — needed by app.scroll_by() too.
    let (cell_w, cell_h) = app.picker.font_size();
    let viewport_w = (area.width as u32) * (cell_w as u32);
    let viewport_h = (area.height as u32) * (cell_h as u32);
    app.viewport_px = (viewport_w, viewport_h);

    // Step 2: compose (overlay + crop), cached.
    let sel_key = app.selection.map(|s| {
        (
            (s.x * 10000.0) as u32,
            (s.y * 10000.0) as u32,
            (s.w * 10000.0) as u32,
            (s.h * 10000.0) as u32,
        )
    });
    let compose_key = ComposeKey {
        pdfium: pdfium_key,
        scroll_x_milli: (app.scroll_x * 10000.0) as u32,
        scroll_y_milli: (app.scroll_y * 10000.0) as u32,
        selection: sel_key,
        selection_color_idx: app.selection_color_idx,
        highlight_count: app.highlights.items.iter().filter(|h| h.page == app.page).count(),
    };
    if app.last_compose_key == Some(compose_key) && app.image_proto.is_some() {
        return Ok(());
    }

    // Clone the cached pixmap into an RgbaImage for in-place blending.
    let base = app.rendered_page.as_ref().expect("rendered_page set above");
    let mut canvas: RgbaImage = base.to_rgba8();
    let (full_w, full_h) = (canvas.width(), canvas.height());

    // Saved highlights for this page → translucent fill, no border.
    for h in app.highlights.for_page(app.page) {
        let rect = norm_to_pixels(
            Rect01 { x: h.x, y: h.y, w: h.w, h: h.h },
            full_w,
            full_h,
        );
        let rgb = rgb_from_hex(&h.color);
        fill_rect_blend(&mut canvas, rect, rgb, 0.35);
    }

    // Active Visual-mode selection → translucent fill plus a 2-px
    // outline so the user can see the edges over a yellow page.
    if let Some(sel) = app.selection {
        let rect = norm_to_pixels(sel, full_w, full_h);
        let color = HIGHLIGHT_COLORS[app.selection_color_idx % HIGHLIGHT_COLORS.len()];
        fill_rect_blend(&mut canvas, rect, color.rgb, 0.30);
        outline_rect(&mut canvas, rect, color.rgb, 2);
    }

    // Crop to viewport at the current scroll offset. crop_rect()
    // gracefully degrades to "no crop" when the rendered page is
    // smaller than the viewport (zoom < 1 or naturally narrow page).
    let (crop_x, crop_y, crop_w, crop_h) = crop_rect(
        full_w, full_h, viewport_w, viewport_h, app.scroll_x, app.scroll_y,
    );
    let composed = if crop_x == 0 && crop_y == 0 && crop_w == full_w && crop_h == full_h {
        DynamicImage::ImageRgba8(canvas)
    } else {
        DynamicImage::ImageRgba8(canvas).crop_imm(crop_x, crop_y, crop_w, crop_h)
    };

    app.image_proto = Some(app.picker.new_resize_protocol(composed));
    app.last_compose_key = Some(compose_key);
    Ok(())
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
            format!(" {}/{}  ", app.page + 1, app.page_count),
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
        if app.scroll_x > 0.001 || app.scroll_y > 0.001 {
            spans.push(Span::styled(
                format!("scroll {:.0}/{:.0}  ", app.scroll_x * 100.0, app.scroll_y * 100.0),
                Style::default().fg(Color::DarkGray),
            ));
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
        "termpdf-rs — vim-style PDF reader",
        "",
        "  j / k / b              next / prev page",
        "  N j  /  N k            jump N pages forward / back",
        "  Space                  scroll a screen, then advance page",
        "  gg / G                 first / last page",
        "  N G                    jump to page N",
        "",
        "  arrows / h / l         scroll within the page",
        "  Ctrl-d / Ctrl-u        scroll half a screen down / up",
        "  mouse wheel            scroll (Shift = horizontal)",
        "",
        "  + / - / 0              zoom in / out / reset",
        "  d                      toggle dark mode (luminance-only)",
        "",
        "  v                      enter Visual mode (highlight)",
        "    hjkl / arrows        move selection",
        "    HJKL / Shift+arrows  resize selection",
        "    c                    cycle highlight color",
        "    y / Enter            save highlight",
        "    Esc                  cancel",
        "  x                      delete last highlight on this page",
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
    let w = 64u16.min(area.width);
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
