//! `:command` parser. Handles bare numbers (vim line-jump), `:q`,
//! `:set [no]dark`, `:goto N`. Unknown commands surface in the
//! status line rather than erroring.

use crate::app::App;

pub fn execute(app: &mut App<'_>, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }

    // `:23` → vim-style page jump, 1-indexed.
    if let Ok(page) = line.parse::<usize>() {
        app.goto_page(page.saturating_sub(1));
        return;
    }

    let mut parts = line.split_whitespace();
    let cmd = parts.next().unwrap_or("");
    match cmd {
        "q" | "quit" => app.should_quit = true,
        "set" => {
            for opt in parts {
                match opt {
                    "dark" => {
                        app.dark = true;
                        app.invalidate();
                    }
                    "nodark" => {
                        app.dark = false;
                        app.invalidate();
                    }
                    _ => app.status = format!("unknown :set option: {opt}"),
                }
            }
        }
        "goto" => {
            if let Some(arg) = parts.next() {
                if let Ok(n) = arg.parse::<usize>() {
                    app.goto_page(n.saturating_sub(1));
                } else {
                    app.status = format!(":goto needs a number, got {arg:?}");
                }
            } else {
                app.status = ":goto needs a page number".into();
            }
        }
        "help" => app.show_help = true,
        _ => app.status = format!("unknown command: {cmd}"),
    }
}
