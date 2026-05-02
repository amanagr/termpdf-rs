use anyhow::Result;
use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::app::{App, Mode};
use crate::cmd;

/// Step sizes. Scroll steps are in fractions of a viewport screen
/// (continuous mode operates in pixels under the hood, but the UX
/// is "how much of a screen does this key move me").
const SCROLL_LINE: f32 = 0.05;
const SCROLL_HALF: f32 = 0.50;
const SELECTION_STEP: f32 = 0.02;

pub fn dispatch(app: &mut App<'_>, k: KeyEvent) -> Result<()> {
    if app.show_help {
        if matches!(
            k.code,
            KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q')
        ) {
            app.show_help = false;
            // Force a full re-encode so the kitty-graphics skip-cells
            // covered by the popup get repainted.
            app.invalidate_compose();
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

        // Page-boundary jumps.
        KeyCode::Char('j') => {
            app.next_page(count.unwrap_or(1));
            app.pending.clear();
        }
        KeyCode::Char('k') => {
            app.prev_page(count.unwrap_or(1));
            app.pending.clear();
        }
        KeyCode::Char('b') => app.prev_page(count.unwrap_or(1)),
        KeyCode::Char(' ') => app.scroll_by_screens(SCROLL_HALF),

        // Within-document scroll. Arrows for fine; Ctrl-d/u for
        // half-screen jumps.
        KeyCode::Down => app.scroll_by_screens(SCROLL_LINE),
        KeyCode::Up => app.scroll_by_screens(-SCROLL_LINE),
        KeyCode::Left => app.scroll_x_by(-SCROLL_LINE),
        KeyCode::Right => app.scroll_x_by(SCROLL_LINE),
        KeyCode::Char('d') if ctrl => app.scroll_by_screens(SCROLL_HALF),
        KeyCode::Char('u') if ctrl => app.scroll_by_screens(-SCROLL_HALF),
        KeyCode::Char('h') if !ctrl => app.scroll_x_by(-SCROLL_LINE),
        KeyCode::Char('l') if !ctrl => app.scroll_x_by(SCROLL_LINE),
        KeyCode::Char('d') => app.toggle_dark(),

        KeyCode::Char('g') => {
            if app.pending == "g" {
                app.first_page();
                app.pending.clear();
            } else {
                app.pending.push('g');
            }
        }
        KeyCode::Char('G') => {
            match count {
                Some(n) => app.goto_page(n.saturating_sub(1)),
                None => app.last_page(),
            }
            app.pending.clear();
        }

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

        KeyCode::Char('x') => {
            if app.delete_last_highlight_on_current_page() {
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

        KeyCode::Char('h') => app.nudge_selection(-SELECTION_STEP, 0.0, false),
        KeyCode::Char('l') => app.nudge_selection(SELECTION_STEP, 0.0, false),
        KeyCode::Char('j') => app.nudge_selection(0.0, SELECTION_STEP, false),
        KeyCode::Char('k') => app.nudge_selection(0.0, -SELECTION_STEP, false),
        KeyCode::Char('H') => app.nudge_selection(-SELECTION_STEP, 0.0, true),
        KeyCode::Char('L') => app.nudge_selection(SELECTION_STEP, 0.0, true),
        KeyCode::Char('J') => app.nudge_selection(0.0, SELECTION_STEP, true),
        KeyCode::Char('K') => app.nudge_selection(0.0, -SELECTION_STEP, true),

        KeyCode::Left => app.nudge_selection(-SELECTION_STEP, 0.0, shift),
        KeyCode::Right => app.nudge_selection(SELECTION_STEP, 0.0, shift),
        KeyCode::Up => app.nudge_selection(0.0, -SELECTION_STEP, shift),
        KeyCode::Down => app.nudge_selection(0.0, SELECTION_STEP, shift),
        _ => {}
    }
    Ok(())
}

fn search_keys(app: &mut App<'_>, k: KeyEvent) -> Result<()> {
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
    let shift = m.modifiers.contains(KeyModifiers::SHIFT);
    match m.kind {
        // Wheel always scrolls — it works in any mode, including
        // Visual, where the user might want to drag past a page edge.
        MouseEventKind::ScrollDown if shift => app.scroll_x_by(SCROLL_LINE),
        MouseEventKind::ScrollUp if shift => app.scroll_x_by(-SCROLL_LINE),
        MouseEventKind::ScrollDown => app.scroll_by_screens(SCROLL_LINE),
        MouseEventKind::ScrollUp => app.scroll_by_screens(-SCROLL_LINE),
        MouseEventKind::ScrollRight => app.scroll_x_by(SCROLL_LINE),
        MouseEventKind::ScrollLeft => app.scroll_x_by(-SCROLL_LINE),

        // Left-click drag = highlight. Click without drag = exit
        // Visual mode (handled by mouse_drag_end's small-rect path).
        MouseEventKind::Down(MouseButton::Left) => app.mouse_drag_start(m.column, m.row),
        MouseEventKind::Drag(MouseButton::Left) => app.mouse_drag_to(m.column, m.row),
        MouseEventKind::Up(MouseButton::Left) => app.mouse_drag_end(),

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
