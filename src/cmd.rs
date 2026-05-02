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
                        app.invalidate_compose();
                    }
                    "nodark" => {
                        app.dark = false;
                        app.invalidate_compose();
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
        "nohl" | "nohlsearch" => app.clear_search(),
        "toc" => app.toggle_toc(),
        "help" => app.show_help = true,
        // `:export PATH` — dump every saved highlight into a Markdown
        // notes file (page-grouped, with quoted text + any inline
        // notes). Path defaults to `<pdf-stem>.notes.md` next to the
        // PDF when omitted.
        "export" | "notes" => {
            let target = parts.next().map(std::path::PathBuf::from).unwrap_or_else(|| {
                let mut p = app.path.clone();
                let stem = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("notes")
                    .to_string();
                p.set_file_name(format!("{stem}.notes.md"));
                p
            });
            match app.export_notes(&target) {
                Ok(()) => app.status = format!("exported notes → {}", target.display()),
                Err(e) => app.status = format!("export failed: {e:#}"),
            }
        }
        _ => app.status = format!("unknown command: {cmd}"),
    }
}
