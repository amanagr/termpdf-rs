//! ratatui frame composition: page image + 1-row status line + ?
//! help overlay. The image itself is rendered by ratatui-image, which
//! handles Kitty/Sixel chunking transparently.

use anyhow::Result;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};

use crate::app::{App, Mode, RenderKey};
use crate::dark;
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
        // Horizontal centering. ratatui-image's Resize::Fit preserves
        // aspect ratio but anchors top-left within the area, so a
        // portrait page in a wide terminal lands flush against the
        // left edge. `size_for` tells us how many cells the image will
        // occupy under Fit; we recompute the x-offset so that empty
        // space splits evenly on both sides. Height stays full because
        // PDFs are taller than wide and the layout already height-binds.
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
    let key = RenderKey {
        page: app.page,
        dark: app.dark,
        area_w: area.width,
        area_h: area.height,
        zoom_milli: (app.zoom * 1000.0) as u32,
    };
    if app.last_render_key == Some(key) && app.image_proto.is_some() {
        return Ok(());
    }

    let img = pdf::render_page(&app.document, app.page, area, &app.picker, app.zoom)?;
    let img = if app.dark {
        image::DynamicImage::ImageRgba8(dark::invert_luminance(&img))
    } else {
        img
    };

    app.image_proto = Some(app.picker.new_resize_protocol(img));
    app.last_render_key = Some(key);
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
        "  j / k / Space / b      next / prev page",
        "  N j  /  N k            jump N pages forward / back",
        "  gg / G                 first / last page",
        "  N G                    jump to page N",
        "  + / - / 0              zoom in / out / reset",
        "  d                      toggle dark mode (luminance-only)",
        "  v ... y                visual-mode highlight (stub)",
        "  /<query>               search (stub)",
        "  :<n>                   jump to page n",
        "  :q                     quit",
        "  :set dark | :set nodark",
        "  ?                      toggle this overlay",
        "  q                      quit",
        "",
        "Press ? or Esc to close",
    ];

    let h = (help_lines.len() as u16 + 4).min(area.height);
    let w = 60u16.min(area.width);
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
