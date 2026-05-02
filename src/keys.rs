use anyhow::Result;
use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::app::{App, Mode};
use crate::cmd;

/// Step sizes. Scroll steps are in fractions of a viewport screen
/// (continuous mode operates in pixels under the hood, but the UX
/// is "how much of a screen does this key move me"). `SCROLL_SCREEN`
/// is intentionally less than 1.0 so the user keeps a sliver of
/// context across a page-down — same trick Vim's `<C-f>` and less'
/// `<space>` use.
const SCROLL_LINE: f32 = 0.05;
const SCROLL_HALF: f32 = 0.50;
const SCROLL_SCREEN: f32 = 0.85;

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

    if app.show_toc {
        return toc_keys(app, k);
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
        // less-style screen scroll. Space pages forward by ~one
        // viewport-sized window (with a sliver of overlap so the
        // user keeps context); b pages back. Pure half-screen jumps
        // live on Ctrl-d / Ctrl-u, matching vim. Previously `b` was
        // a duplicate of `k` (prev page boundary); the new binding
        // is what users coming from `less`/`man` expect.
        KeyCode::Char(' ') => {
            app.scroll_by_screens(SCROLL_SCREEN);
            app.pending.clear();
        }
        KeyCode::Char('b') => {
            app.scroll_by_screens(-SCROLL_SCREEN);
            app.pending.clear();
        }

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

        KeyCode::Char('n') => app.advance_search(1),
        KeyCode::Char('N') => app.advance_search(-1),

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
        KeyCode::Char('o') => app.toggle_toc(),

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
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

    // f-pending: previous keypress was `f`/`F`, awaiting the target
    // char. Stored as a single `f` or `F` in `pending`.
    if app.pending == "f" || app.pending == "F" {
        let forward = app.pending == "f";
        app.pending.clear();
        if let KeyCode::Char(c) = k.code {
            app.move_head_find_char(c, forward);
        }
        return Ok(());
    }
    // i-pending: previous keypress was `i`, awaiting text-object
    // (`iw`/`is`/`ip`).
    if app.pending == "i" {
        app.pending.clear();
        match k.code {
            KeyCode::Char('w') => app.select_inner_word(),
            KeyCode::Char('s') => app.select_inner_sentence(),
            KeyCode::Char('p') => app.select_inner_paragraph(),
            _ => {}
        }
        return Ok(());
    }
    // gg-pending: previous was `g`, awaiting `g`.
    if app.pending == "g" {
        app.pending.clear();
        if let KeyCode::Char('g') = k.code {
            app.move_head_page_top();
        }
        return Ok(());
    }

    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => app.exit_visual(),
        KeyCode::Char('y') | KeyCode::Enter => app.yank_selection(true),
        KeyCode::Char('Y') => app.yank_selection(false),
        KeyCode::Char('c') => app.cycle_color(),

        // Char-wise caret motion (vim's `h`/`l`).
        KeyCode::Char('h') | KeyCode::Left => app.move_head_chars(-1),
        KeyCode::Char('l') | KeyCode::Right => app.move_head_chars(1),
        // Line-wise caret motion (`j`/`k`); column-preserving.
        KeyCode::Char('j') | KeyCode::Down => app.move_head_lines(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_head_lines(-1),

        // Word motions.
        KeyCode::Char('w') => app.move_head_word_forward(),
        KeyCode::Char('b') => app.move_head_word_back(),
        KeyCode::Char('e') => app.move_head_word_end(),

        // Line-extreme + page-extreme motions.
        KeyCode::Char('0') => app.move_head_line_start(),
        KeyCode::Char('^') => app.move_head_line_first_nonblank(),
        KeyCode::Char('$') => app.move_head_line_end(),
        KeyCode::Char('G') => app.move_head_page_bottom(),
        KeyCode::Char('g') => app.pending.push('g'),

        // f<c>/F<c> — start a one-char wait for the target.
        KeyCode::Char('f') => app.pending.push('f'),
        KeyCode::Char('F') => app.pending.push('F'),

        // Text objects: `i` then w/s/p.
        KeyCode::Char('i') => app.pending.push('i'),

        // Visual mode flavours: V switches to linewise, <C-v> to
        // blockwise. They modify the active selection's mode rather
        // than re-entering — the anchor stays put.
        KeyCode::Char('V') => app.enter_visual_line(),
        KeyCode::Char('v') if ctrl => app.enter_visual_block(),

        _ => {}
    }
    Ok(())
}

fn toc_keys(app: &mut App<'_>, k: KeyEvent) -> Result<()> {
    // Filter-edit mode: every printable goes into the buffer; Enter
    // commits the filter, Esc cancels.
    if app.toc_filter_editing {
        match k.code {
            KeyCode::Esc => {
                app.toc_filter.clear();
                app.toc_filter_finish();
            }
            KeyCode::Enter => app.toc_filter_finish(),
            KeyCode::Backspace => app.toc_filter_pop(),
            KeyCode::Char(c) => app.toc_filter_push(c),
            _ => {}
        }
        return Ok(());
    }

    // Navigation mode.
    match k.code {
        KeyCode::Esc | KeyCode::Char('o') | KeyCode::Char('q') => app.toggle_toc(),
        KeyCode::Char('j') | KeyCode::Down => app.toc_move(1),
        KeyCode::Char('k') | KeyCode::Up => app.toc_move(-1),
        KeyCode::Char('g') => app.toc_jump_to_top(),
        KeyCode::Char('G') => app.toc_jump_to_bottom(),
        KeyCode::Enter => app.toc_activate(),
        KeyCode::Char('/') => app.toc_filter_start(),
        _ => {}
    }
    Ok(())
}

fn search_keys(app: &mut App<'_>, k: KeyEvent) -> Result<()> {
    match k.code {
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.cmd_buffer.clear();
            app.status.clear();
        }
        KeyCode::Enter => {
            let buf = std::mem::take(&mut app.cmd_buffer);
            app.mode = Mode::Normal;
            app.run_search(&buf);
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

pub fn parse_count(pending: &str) -> Option<usize> {
    if pending.is_empty() {
        return None;
    }
    let digits: String = pending.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::parse_count;

    #[test]
    fn parse_count_empty_is_none() {
        assert_eq!(parse_count(""), None);
    }

    #[test]
    fn parse_count_digits_only() {
        assert_eq!(parse_count("42"), Some(42));
    }

    #[test]
    fn parse_count_pure_letters_is_none() {
        // `gg` first-stroke buffers a 'g'; parse_count must say "no count".
        assert_eq!(parse_count("g"), None);
    }

    #[test]
    fn parse_count_strips_letters_around_digits() {
        // `5g` (illegal sequence today, but make the filter contract
        // explicit so future refactors don't drift): the digits are
        // extracted, letters dropped.
        assert_eq!(parse_count("5g"), Some(5));
    }

    #[test]
    fn parse_count_overflow_is_none() {
        assert_eq!(parse_count("999999999999999999999"), None);
    }
}
