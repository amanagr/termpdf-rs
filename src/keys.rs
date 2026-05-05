use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::app::{App, Mode};
use crate::cmd;

/// Step sizes. Scroll steps are in fractions of a viewport screen
/// (continuous mode operates in pixels under the hood, but the UX
/// is "how much of a screen does this key move me"). `SCROLL_SCREEN`
/// is intentionally less than 1.0 so the user keeps a sliver of
/// context across a page-down — same trick Vim's `<C-f>` and less'
/// `<space>` use.
pub(crate) const SCROLL_LINE: f32 = 0.05;
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

    // Link-hint mode hijacks every keystroke until the user picks a
    // hint (or cancels). Handled before mode dispatch so it overrides
    // every other binding.
    if app.link_hint_mode {
        match k.code {
            KeyCode::Esc => {
                app.exit_link_hint_mode();
                app.status = "link mode cancelled".into();
            }
            KeyCode::Char(c) => {
                if let Some(action) = app.hint_keystroke(c) {
                    app.follow_link_action(action);
                }
            }
            _ => { /* ignore other keys */ }
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
    // Highlight-delete confirm: `x` stages, the very next keypress
    // either confirms with `y` or cancels with anything else.
    // Handled BEFORE mode dispatch so the confirm-`y` doesn't fall
    // through to other `y` bindings later.
    if app.awaiting_highlight_delete_confirm {
        if matches!(k.code, KeyCode::Char('y')) {
            let removed = app.confirm_delete_last_highlight();
            app.status = if removed > 0 {
                format!(
                    "deleted highlight ({removed} rect{})",
                    if removed == 1 { "" } else { "s" }
                )
            } else {
                "nothing deleted".into()
            };
        } else {
            app.cancel_delete_last_highlight();
            app.status = "delete cancelled".into();
        }
        return Ok(());
    }

    // Mark-set / mark-jump consume the very next keystroke as the
    // mark name. Handled before the regular dispatch so a literal
    // `j`/`k`/`q` here lands in the BTreeMap instead of moving pages.
    if app.awaiting_mark_set {
        app.awaiting_mark_set = false;
        if let KeyCode::Char(c) = k.code {
            app.set_mark(c);
        }
        return Ok(());
    }
    if app.awaiting_mark_jump {
        app.awaiting_mark_jump = false;
        if let KeyCode::Char(c) = k.code {
            app.jump_to_mark(c);
        }
        return Ok(());
    }

    let count = parse_count(&app.pending);
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);

    match k.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('?') => app.show_help = !app.show_help,

        // Page-boundary jumps. Throttled — Linux key autorepeat fires
        // ~30 Hz, which would flip 30 PDF pages/sec; `note_scroll_attempt`
        // caps that at ~6.7 Hz (a brisk reading cadence) and silently
        // drops the over-rate keypresses. Counted forms (e.g. `5j`)
        // bypass the throttle since they're a single intentional action.
        KeyCode::Char('j') => {
            if count.is_some() || app.note_scroll_attempt() {
                app.next_page(count.unwrap_or(1));
            }
            app.pending.clear();
        }
        KeyCode::Char('k') => {
            if count.is_some() || app.note_scroll_attempt() {
                app.prev_page(count.unwrap_or(1));
            }
            app.pending.clear();
        }
        // less-style screen scroll. Space pages forward by ~one
        // viewport-sized window (with a sliver of overlap so the
        // user keeps context); b pages back. Pure half-screen jumps
        // live on Ctrl-d / Ctrl-u, matching vim. Previously `b` was
        // a duplicate of `k` (prev page boundary); the new binding
        // is what users coming from `less`/`man` expect.
        KeyCode::Char(' ') => {
            if count.is_some() || app.note_scroll_attempt() {
                let n = count.unwrap_or(1).max(1);
                for _ in 0..n {
                    app.scroll_by_screens(SCROLL_SCREEN);
                }
            }
            app.pending.clear();
        }
        KeyCode::Char('b') => {
            if app.note_scroll_attempt() {
                app.scroll_by_screens(-SCROLL_SCREEN);
            }
            app.pending.clear();
        }

        // Within-document scroll. Arrows for fine; Ctrl-d/u for
        // half-screen jumps. All throttled.
        KeyCode::Down if app.note_scroll_attempt() => {
            app.scroll_by_screens(SCROLL_LINE);
        }
        KeyCode::Up if app.note_scroll_attempt() => {
            app.scroll_by_screens(-SCROLL_LINE);
        }
        KeyCode::Left => app.scroll_x_by(-SCROLL_LINE),
        KeyCode::Right => app.scroll_x_by(SCROLL_LINE),
        KeyCode::Char('d') if ctrl && app.note_scroll_attempt() => {
            app.scroll_by_screens(SCROLL_HALF);
        }
        KeyCode::Char('u') if ctrl && app.note_scroll_attempt() => {
            app.scroll_by_screens(-SCROLL_HALF);
        }
        // Vim-style jumplist: <C-o> back, <C-i> / Tab forward.
        KeyCode::Char('o') if ctrl => app.jump_back(),
        KeyCode::Char('i') if ctrl => app.jump_forward(),
        KeyCode::Tab => app.jump_forward(),
        KeyCode::Char('h') if !ctrl => app.scroll_x_by(-SCROLL_LINE),
        // Manual refresh: re-transmit every cached page on the next
        // frame. Recovers from Ghostty's internal image-cache eviction
        // (image-storage-limit, default 320 MB) which silently drops
        // our images and leaves placeholder cells pointing at freed
        // image_ids — the page renders blank until something forces a
        // re-transmit. Same effect as the user's zoom-out + zoom-in
        // workaround, without the layout/scroll churn.
        KeyCode::Char('l') if ctrl => {
            crate::debug_log::write("ctrl_l", "user pressed Ctrl-L");
            if let Some(kp) = app.kitty_pages.as_mut() {
                kp.invalidate_all_transmits();
            }
            app.invalidate_compose();
            app.status = "refreshed".into();
        }
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
            // Two-stroke confirm: first `x` stages the delete and
            // shows the count in the status line. The next keypress
            // either confirms with `y` or cancels with anything else.
            // Prevents the foot-cannon where one stray `x` wipes a
            // multi-line highlight the user just made.
            let n = app.request_delete_last_highlight();
            if n == 0 {
                app.status = "no highlights on this page".into();
            } else {
                app.status = format!(
                    "delete highlight ({n} rect{})? press y to confirm, any other key cancels",
                    if n == 1 { "" } else { "s" }
                );
            }
        }

        KeyCode::Char(':') => {
            app.clear_chord_state();
            app.mode = Mode::Command;
            app.cmd_buffer.clear();
        }
        KeyCode::Char('/') => {
            app.clear_chord_state();
            app.mode = Mode::Search;
            app.cmd_buffer.clear();
            app.status = "Search: (typing — Enter to run, Esc to cancel)".into();
        }
        KeyCode::Char('v') => app.enter_visual(),
        KeyCode::Char('o') => app.toggle_toc(),
        // `f` enters link-hint mode (vimium-style): every clickable
        // link on visible pages gets a 1-2 char overlay; type to pick.
        KeyCode::Char('f') => app.enter_link_hint_mode(),
        // `]]` / `[[` jump to next / prev outline entry. Both
        // require a doubled keystroke matching vim's section-motion
        // binding. The first ] / [ stays in `pending`; the second
        // fires the jump.
        KeyCode::Char(']') => {
            if app.pending == "]" {
                app.jump_section(1);
                app.pending.clear();
            } else {
                app.pending.push(']');
            }
        }
        KeyCode::Char('[') => {
            if app.pending == "[" {
                app.jump_section(-1);
                app.pending.clear();
            } else {
                app.pending.push('[');
            }
        }

        // Marks: `m{a-z}` to set, `'{a-z}` to jump. Two-stroke pattern
        // matched in the awaiting_mark_* prelude above.
        KeyCode::Char('m') => app.awaiting_mark_set = true,
        KeyCode::Char('\'') => app.awaiting_mark_jump = true,

        KeyCode::Esc => {
            // Drop pending chord, mark-pending flags, and any other
            // half-typed Normal-mode state. Otherwise `m<Esc>` leaves
            // awaiting_mark_set true and silently consumes the next
            // keystroke as the mark name.
            app.clear_chord_state();
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
        KeyCode::Backspace if app.cmd_buffer.pop().is_none() => {
            app.mode = Mode::Normal;
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
            // In placement mode the anchor must follow the head so
            // motions don't silently grow a selection — same contract
            // as the unified motion arms below.
            app.sync_anchor_to_head_if_placing();
            app.scroll_to_head_if_offscreen();
        }
        return Ok(());
    }
    // i-pending: previous keypress was `i`, awaiting text-object
    // (`iw`/`is`/`ip`). Text-objects explicitly set both anchor and
    // head, so no sync_anchor call is needed here.
    if app.pending == "i" {
        app.pending.clear();
        match k.code {
            KeyCode::Char('w') => app.select_inner_word(),
            KeyCode::Char('s') => app.select_inner_sentence(),
            KeyCode::Char('p') => app.select_inner_paragraph(),
            _ => {}
        }
        app.scroll_to_head_if_offscreen();
        return Ok(());
    }
    // g-pending: previous was `g`. Awaits `g` (page-top) or `y`
    // (yank-as-markdown). Anything else cancels.
    if app.pending == "g" {
        app.pending.clear();
        match k.code {
            KeyCode::Char('g') => {
                app.move_head_page_top();
                app.sync_anchor_to_head_if_placing();
                app.scroll_to_head_if_offscreen();
            }
            KeyCode::Char('y') => app.yank_selection_as_markdown(),
            _ => {}
        }
        return Ok(());
    }

    // Track whether this dispatch moved the head — if so, follow up
    // with scroll_to_head_if_offscreen so the user always sees what
    // they're selecting. Captures all caret-motion arms below in one
    // place rather than scattering the call across each.
    let mut moved_head = false;

    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => app.exit_visual(),
        KeyCode::Char('y') | KeyCode::Enter => app.yank_selection(true),
        KeyCode::Char('Y') => app.yank_selection(false),
        KeyCode::Char('c') => app.cycle_color(),

        // Char-wise caret motion (vim's `h`/`l`).
        KeyCode::Char('h') | KeyCode::Left => {
            app.move_head_chars(-1);
            moved_head = true;
        }
        KeyCode::Char('l') | KeyCode::Right => {
            app.move_head_chars(1);
            moved_head = true;
        }
        // Line-wise caret motion (`j`/`k`); column-preserving.
        KeyCode::Char('j') | KeyCode::Down => {
            app.move_head_lines(1);
            moved_head = true;
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.move_head_lines(-1);
            moved_head = true;
        }

        // Word motions.
        KeyCode::Char('w') => {
            app.move_head_word_forward();
            moved_head = true;
        }
        KeyCode::Char('b') => {
            app.move_head_word_back();
            moved_head = true;
        }
        KeyCode::Char('e') => {
            app.move_head_word_end();
            moved_head = true;
        }

        // Line-extreme + page-extreme motions.
        KeyCode::Char('0') => {
            app.move_head_line_start();
            moved_head = true;
        }
        KeyCode::Char('^') => {
            app.move_head_line_first_nonblank();
            moved_head = true;
        }
        KeyCode::Char('$') => {
            app.move_head_line_end();
            moved_head = true;
        }
        KeyCode::Char('G') => {
            app.move_head_page_bottom();
            moved_head = true;
        }
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
        // `v` toggles placement vs selection. Entering Visual lands
        // in placement so the user can position the caret first; the
        // first `v` here locks the anchor and starts growing the band.
        // A second `v` reverts to placement so they can relocate.
        KeyCode::Char('v') => app.toggle_selection_placement(),

        _ => {}
    }
    if moved_head {
        // In placement mode the band must stay collapsed to a single
        // char — drag the anchor along with the head so both move
        // together. Once the user presses `v` to lock, this becomes
        // a no-op and motions grow the selection from the lock point.
        app.sync_anchor_to_head_if_placing();
        app.scroll_to_head_if_offscreen();
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
    // Fold digits directly into a usize accumulator instead of
    // allocating a filtered String + reparsing it. Saves a small alloc
    // per Normal-mode key dispatch (parse_count fires on every j/k/h/l
    // etc to extract the count prefix).
    let mut acc: usize = 0;
    let mut any = false;
    for c in pending.chars() {
        if let Some(d) = c.to_digit(10) {
            any = true;
            acc = acc.checked_mul(10)?.checked_add(d as usize)?;
        }
    }
    any.then_some(acc)
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
