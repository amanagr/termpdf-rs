use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};

use crate::app::{App, Mode};
use crate::cmd;

/// Step sizes (fraction of a screen / page) for scrolling and
/// selection moves. Tuned for "feels responsive but not jumpy".
const SCROLL_STEP: f32 = 0.10;
const SCROLL_HALF: f32 = 0.50;
const SELECTION_STEP: f32 = 0.02;

pub fn dispatch(app: &mut App<'_>, k: KeyEvent) -> Result<()> {
    if app.show_help {
        // In help-overlay mode any key (Esc, q, ?) just dismisses it.
        if matches!(
            k.code,
            KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q')
        ) {
            app.show_help = false;
            // The kitty-graphics protocol writes a row's full escape
            // sequence into the first cell of each image row and marks
            // every other cell `set_skip(true)`. When the help overlay
            // drew text over those skip-cells, ratatui's diff engine
            // never repaints them on the next frame (skip=true == "don't
            // emit"), so help-text glyphs stay until the next page flip.
            // Invalidating forces a full re-encode → fresh transmit
            // sequence → the terminal repaints the entire image area.
            app.invalidate();
        }
        return Ok(());
    }

    match app.mode {
        Mode::Normal => normal_keys(app, k),
        Mode::Command => cmd_keys(app, k),
        Mode::Visual => visual_keys(app, k),
        Mode::Search => search_keys(app, k),
    }
}

fn normal_keys(app: &mut App<'_>, k: KeyEvent) -> Result<()> {
    let count = parse_count(&app.pending);
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

    match k.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('?') => app.show_help = !app.show_help,

        // Page navigation. Space falls through to scroll-then-page so
        // you can scroll a tall page and roll into the next one with
        // the same key, the way zathura/evince behave.
        KeyCode::Char('j') => {
            app.next_page(count.unwrap_or(1));
            app.pending.clear();
        }
        KeyCode::Char('k') | KeyCode::Char('b') => {
            app.prev_page(count.unwrap_or(1));
            app.pending.clear();
        }
        KeyCode::Char(' ') => {
            // Scroll one screen; if already at bottom, advance page.
            let before = app.scroll_y;
            app.scroll_by(0.0, SCROLL_HALF);
            if (app.scroll_y - before).abs() < f32::EPSILON {
                app.next_page(1);
            }
        }

        // Within-page scroll. Arrows for fine, Ctrl-d/u for half-page.
        KeyCode::Down => app.scroll_by(0.0, SCROLL_STEP),
        KeyCode::Up => app.scroll_by(0.0, -SCROLL_STEP),
        KeyCode::Left => app.scroll_by(-SCROLL_STEP, 0.0),
        KeyCode::Right => app.scroll_by(SCROLL_STEP, 0.0),
        KeyCode::Char('d') if ctrl => app.scroll_by(0.0, SCROLL_HALF),
        KeyCode::Char('u') if ctrl => app.scroll_by(0.0, -SCROLL_HALF),
        KeyCode::Char('d') => app.toggle_dark(),
        KeyCode::Char('h') if !ctrl => app.scroll_by(-SCROLL_STEP, 0.0),
        KeyCode::Char('l') if !ctrl => app.scroll_by(SCROLL_STEP, 0.0),

        KeyCode::Char('g') => {
            // `gg` jumps to first page. The first 'g' just buffers.
            if app.pending == "g" {
                app.first_page();
                app.pending.clear();
            } else {
                app.pending.push('g');
            }
        }
        KeyCode::Char('G') => {
            // Bare `G` → last page. With a count prefix, `23G` → 23.
            match count {
                Some(n) => app.goto_page(n.saturating_sub(1)),
                None => app.last_page(),
            }
            app.pending.clear();
        }

        // `0` is overloaded vim-style: it's a count digit when
        // something's already in `pending`, otherwise it's "fit-page"
        // (zoom = 1.0, scroll reset). Keep it before the 1-9 catchall.
        KeyCode::Char('0') => {
            if app.pending.is_empty() {
                app.zoom_reset();
            } else {
                app.pending.push('0');
            }
        }
        KeyCode::Char(c @ '1'..='9') => app.pending.push(c),

        KeyCode::Char('+') | KeyCode::Char('=') => app.zoom_by(1.25),
        KeyCode::Char('-') => app.zoom_by(1.0 / 1.25),

        // Highlight management.
        KeyCode::Char('x') => {
            if app.delete_last_highlight() {
                app.status = "removed last highlight on this page".into();
            } else {
                app.status = "no highlights on this page".into();
            }
        }

        KeyCode::Char(':') => {
            app.mode = Mode::Command;
            app.cmd_buffer.clear();
        }
        KeyCode::Char('/') => {
            app.mode = Mode::Search;
            app.cmd_buffer.clear();
            app.status = "Search: (typing — Enter to run, Esc to cancel)".into();
        }
        KeyCode::Char('v') => app.enter_visual(),

        KeyCode::Esc => {
            app.pending.clear();
            app.status.clear();
        }
        _ => {}
    }
    Ok(())
}

fn cmd_keys(app: &mut App<'_>, k: KeyEvent) -> Result<()> {
    match k.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.cmd_buffer.clear();
        }
        KeyCode::Enter => {
            let buf = std::mem::take(&mut app.cmd_buffer);
            cmd::execute(app, &buf);
            app.mode = Mode::Normal;
        }
        KeyCode::Backspace => {
            if app.cmd_buffer.pop().is_none() {
                app.mode = Mode::Normal;
            }
        }
        KeyCode::Char(c) => app.cmd_buffer.push(c),
        _ => {}
    }
    Ok(())
}

fn visual_keys(app: &mut App<'_>, k: KeyEvent) -> Result<()> {
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => app.exit_visual(),
        KeyCode::Char('y') | KeyCode::Enter => app.save_selection(),
        KeyCode::Char('c') => app.cycle_color(),

        // hjkl moves the rectangle; uppercase HJKL resizes from the
        // bottom-right corner. Crossterm reports the shifted form as
        // `Char('H')` *and* sets the SHIFT modifier — we match on the
        // uppercase code so terminals that normalise either way work.
        KeyCode::Char('h') => app.nudge_selection(-SELECTION_STEP, 0.0, false),
        KeyCode::Char('l') => app.nudge_selection(SELECTION_STEP, 0.0, false),
        KeyCode::Char('j') => app.nudge_selection(0.0, SELECTION_STEP, false),
        KeyCode::Char('k') => app.nudge_selection(0.0, -SELECTION_STEP, false),
        KeyCode::Char('H') => app.nudge_selection(-SELECTION_STEP, 0.0, true),
        KeyCode::Char('L') => app.nudge_selection(SELECTION_STEP, 0.0, true),
        KeyCode::Char('J') => app.nudge_selection(0.0, SELECTION_STEP, true),
        KeyCode::Char('K') => app.nudge_selection(0.0, -SELECTION_STEP, true),

        // Arrow keys mirror hjkl for users who haven't internalised
        // the vim variants yet. Shift+arrow = resize.
        KeyCode::Left => app.nudge_selection(-SELECTION_STEP, 0.0, shift),
        KeyCode::Right => app.nudge_selection(SELECTION_STEP, 0.0, shift),
        KeyCode::Up => app.nudge_selection(0.0, -SELECTION_STEP, shift),
        KeyCode::Down => app.nudge_selection(0.0, SELECTION_STEP, shift),
        _ => {}
    }
    Ok(())
}

fn search_keys(app: &mut App<'_>, k: KeyEvent) -> Result<()> {
    // v0.1: collect the query but don't actually search yet. Pdfium's
    // text-extract API lands in v0.2.
    match k.code {
        KeyCode::Esc | KeyCode::Enter => {
            if matches!(k.code, KeyCode::Enter) && !app.cmd_buffer.is_empty() {
                app.status = format!("Search '{}' — not yet implemented", app.cmd_buffer);
            }
            app.mode = Mode::Normal;
            app.cmd_buffer.clear();
        }
        KeyCode::Char(c) => app.cmd_buffer.push(c),
        KeyCode::Backspace => {
            app.cmd_buffer.pop();
        }
        _ => {}
    }
    Ok(())
}

pub fn dispatch_mouse(app: &mut App<'_>, m: MouseEvent) -> Result<()> {
    // Mouse wheel scrolls the zoomed page. Shift+wheel scrolls
    // horizontally — same convention as most browsers / GUIs.
    let shift = m.modifiers.contains(KeyModifiers::SHIFT);
    match m.kind {
        MouseEventKind::ScrollDown if shift => app.scroll_by(SCROLL_STEP, 0.0),
        MouseEventKind::ScrollUp if shift => app.scroll_by(-SCROLL_STEP, 0.0),
        MouseEventKind::ScrollDown => app.scroll_by(0.0, SCROLL_STEP),
        MouseEventKind::ScrollUp => app.scroll_by(0.0, -SCROLL_STEP),
        MouseEventKind::ScrollRight => app.scroll_by(SCROLL_STEP, 0.0),
        MouseEventKind::ScrollLeft => app.scroll_by(-SCROLL_STEP, 0.0),
        _ => {}
    }
    Ok(())
}

fn parse_count(pending: &str) -> Option<usize> {
    if pending.is_empty() {
        return None;
    }
    let digits: String = pending.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}
