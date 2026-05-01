use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};

use crate::app::{App, Mode};
use crate::cmd;

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
    match k.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('?') => app.show_help = !app.show_help,
        KeyCode::Char('d') => app.toggle_dark(),

        KeyCode::Char('j') | KeyCode::Down | KeyCode::Char(' ') => {
            app.next_page(count.unwrap_or(1));
            app.pending.clear();
        }
        KeyCode::Char('k') | KeyCode::Up | KeyCode::Char('b') => {
            app.prev_page(count.unwrap_or(1));
            app.pending.clear();
        }

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
        // something's already in `pending`, otherwise it's "fit-width"
        // (zoom = 1.0). Keeping it before the 1-9 catchall so the
        // empty-pending branch wins.
        KeyCode::Char('0') => {
            if app.pending.is_empty() {
                app.zoom = 1.0;
                app.invalidate();
            } else {
                app.pending.push('0');
            }
        }
        KeyCode::Char(c @ '1'..='9') => app.pending.push(c),

        KeyCode::Char('+') | KeyCode::Char('=') => {
            app.zoom = (app.zoom * 1.1).min(5.0);
            app.invalidate();
        }
        KeyCode::Char('-') => {
            app.zoom = (app.zoom / 1.1).max(0.25);
            app.invalidate();
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
        KeyCode::Char('v') => {
            app.mode = Mode::Visual;
            app.status = "VISUAL — j/k/h/l to size, y to highlight, Esc to cancel".into();
        }

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
    // v0.1: just the entry/exit edges. Region growth + drag-to-select
    // and the actual "y" → save-highlight wiring lands in v0.2 once
    // we plumb mouse coords back into PDF page-space.
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = Mode::Normal;
            app.status.clear();
        }
        KeyCode::Char('y') => {
            app.status = "Highlight saved (stub)".into();
            app.mode = Mode::Normal;
        }
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

pub fn dispatch_mouse(_app: &mut App<'_>, _m: MouseEvent) -> Result<()> {
    // Wired up so v0.2 can drop drag-to-highlight in without touching
    // the run-loop plumbing.
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
