//! `:command` parser. Handles bare numbers (vim line-jump), `:q`,
//! `:set [no]dark`, `:goto N`. Unknown commands surface in the
//! status line rather than erroring.
//!
//! The parser is deliberately a flat string match (no clap, no
//! shellwords): the surface is small, no quoting is needed, and the
//! cost of "complete pdf path with spaces" arguments is paid by
//! `:export PATH` only — which we wrap in a saturating split rather
//! than a full quoted-arg parser.

use std::path::PathBuf;

use crate::app::App;

/// Parsed `:`-command. Pure data — testable without an `App` (and
/// therefore without pdfium loaded). `execute` is a thin dispatcher
/// over this enum.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Empty,
    Quit,
    /// 0-indexed page after vim's 1-indexed input was decremented.
    Goto(usize),
    SetDark(bool),
    /// `:set` with at least one unrecognised option. Carries the
    /// first such option for the status-line message.
    SetUnknown(String),
    Nohl,
    Toc,
    Help,
    /// Jump to the document's references / bibliography section if
    /// the outline contains one. No-op with informative status if
    /// the outline doesn't include such a section.
    Refs,
    /// `:export [path]`. `None` means "derive default path from the
    /// open PDF" (resolved at execute-time so the parser stays pure).
    Export(Option<PathBuf>),
    /// Show PDF metadata (title, author, page count, file size).
    Info,
    /// Show rendering diagnostics (cell pixel size, viewport, zoom,
    /// fit-width, render scale). Useful for debugging blur / sizing
    /// issues on a new terminal.
    Diag,
    /// `:goto` with no number, or with a non-numeric arg.
    GotoMissingArg,
    GotoBadArg(String),
    Unknown(String),
}

/// Pure parse: input `&str` → `Command`. Tested in isolation.
pub fn parse(line: &str) -> Command {
    let line = line.trim();
    if line.is_empty() {
        return Command::Empty;
    }

    // `:23` → vim-style page jump, 1-indexed.
    if let Ok(page) = line.parse::<usize>() {
        return Command::Goto(page.saturating_sub(1));
    }

    let mut parts = line.split_whitespace();
    let head = parts.next().unwrap_or("");
    match head {
        "q" | "quit" => Command::Quit,
        "set" => {
            // Last-write-wins on dark/nodark; first unknown opt wins
            // for the error reporting (matches vim's :set behaviour
            // where the first invalid arg is the one called out).
            let mut dark: Option<bool> = None;
            for opt in parts {
                match opt {
                    "dark" => dark = Some(true),
                    "nodark" => dark = Some(false),
                    other => return Command::SetUnknown(other.to_string()),
                }
            }
            match dark {
                Some(b) => Command::SetDark(b),
                // `:set` alone is a no-op rather than an error so a
                // user typing `:set ` and Tab-completing later doesn't
                // get yelled at.
                None => Command::Empty,
            }
        }
        "goto" => match parts.next() {
            None => Command::GotoMissingArg,
            Some(arg) => match arg.parse::<usize>() {
                Ok(n) => Command::Goto(n.saturating_sub(1)),
                Err(_) => Command::GotoBadArg(arg.to_string()),
            },
        },
        "nohl" | "noh" | "nohlsearch" => Command::Nohl,
        "toc" => Command::Toc,
        "help" => Command::Help,
        "refs" | "ref" | "bib" | "bibliography" | "references" => Command::Refs,
        "info" | "metadata" => Command::Info,
        "diag" | "diagnostics" => Command::Diag,
        "export" | "notes" => {
            // `:export some/path/with spaces.md` is rare but possible;
            // take the rest of the line verbatim rather than re-joining
            // split tokens. The previous `parts.collect().join(" ")`
            // collapsed runs of whitespace and tabs (so `:export  /tmp/a  b.md`
            // silently became `/tmp/a b.md`, writing to the wrong file).
            let head_end = head.len();
            let rest = line.get(head_end..).unwrap_or("").trim();
            let target = if rest.is_empty() {
                None
            } else {
                Some(PathBuf::from(rest))
            };
            Command::Export(target)
        }
        other => Command::Unknown(other.to_string()),
    }
}

pub fn execute(app: &mut App<'_>, line: &str) {
    match parse(line) {
        Command::Empty => {}
        Command::Quit => app.should_quit = true,
        Command::Goto(p) => app.goto_page(p),
        Command::SetDark(b) => {
            app.dark = b;
            app.invalidate_compose();
        }
        Command::SetUnknown(opt) => app.status = format!("unknown :set option: {opt}"),
        Command::Nohl => app.clear_search(),
        Command::Toc => app.toggle_toc(),
        Command::Help => app.show_help = true,
        Command::Refs => app.jump_to_references(),
        Command::Info => app.show_info(),
        Command::Diag => app.show_diag(),
        Command::Export(maybe_target) => {
            let target = maybe_target.unwrap_or_else(|| {
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
                Ok(()) => {
                    app.status = format!(
                        "exported notes → {}",
                        crate::term_safe::safe_for_stderr(&target.display().to_string())
                    )
                }
                Err(e) => {
                    app.status = format!(
                        "export failed: {}",
                        crate::term_safe::safe_for_stderr(&format!("{e:#}"))
                    )
                }
            }
        }
        Command::GotoMissingArg => app.status = ":goto needs a page number".into(),
        Command::GotoBadArg(arg) => {
            app.status = format!(
                ":goto needs a number, got {:?}",
                crate::term_safe::safe_for_stderr(&arg)
            )
        }
        Command::Unknown(cmd) => {
            app.status = format!(
                "unknown command: {}",
                crate::term_safe::safe_for_stderr(&cmd)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_whitespace_are_noops() {
        assert_eq!(parse(""), Command::Empty);
        assert_eq!(parse("   "), Command::Empty);
    }

    #[test]
    fn bare_number_is_one_indexed_goto() {
        assert_eq!(parse("23"), Command::Goto(22));
        assert_eq!(parse("1"), Command::Goto(0));
        // `:0` saturates to first page rather than wrapping.
        assert_eq!(parse("0"), Command::Goto(0));
    }

    #[test]
    fn quit_aliases() {
        assert_eq!(parse("q"), Command::Quit);
        assert_eq!(parse("quit"), Command::Quit);
        assert_eq!(parse("  q  "), Command::Quit);
    }

    #[test]
    fn set_dark_and_nodark() {
        assert_eq!(parse("set dark"), Command::SetDark(true));
        assert_eq!(parse("set nodark"), Command::SetDark(false));
        // Last write wins.
        assert_eq!(parse("set dark nodark"), Command::SetDark(false));
    }

    #[test]
    fn set_unknown_option_reports_name() {
        match parse("set bogus") {
            Command::SetUnknown(s) => assert_eq!(s, "bogus"),
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn bare_set_is_noop_not_error() {
        assert_eq!(parse("set"), Command::Empty);
    }

    #[test]
    fn goto_with_number() {
        assert_eq!(parse("goto 5"), Command::Goto(4));
    }

    #[test]
    fn goto_missing_or_bad_arg() {
        assert_eq!(parse("goto"), Command::GotoMissingArg);
        match parse("goto abc") {
            Command::GotoBadArg(s) => assert_eq!(s, "abc"),
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn nohl_aliases() {
        assert_eq!(parse("nohl"), Command::Nohl);
        // :noh is vim's unique-prefix abbreviation; users will type it.
        assert_eq!(parse("noh"), Command::Nohl);
        assert_eq!(parse("nohlsearch"), Command::Nohl);
    }

    #[test]
    fn export_with_and_without_path() {
        assert_eq!(parse("export"), Command::Export(None));
        assert_eq!(parse("notes"), Command::Export(None));
        assert_eq!(
            parse("export /tmp/notes.md"),
            Command::Export(Some(PathBuf::from("/tmp/notes.md")))
        );
        // Path with spaces survives — collected verbatim, not split.
        assert_eq!(
            parse("export /tmp/has spaces.md"),
            Command::Export(Some(PathBuf::from("/tmp/has spaces.md")))
        );
    }

    #[test]
    fn unknown_command_carries_name() {
        match parse("frobnicate") {
            Command::Unknown(s) => assert_eq!(s, "frobnicate"),
            other => panic!("got {:?}", other),
        }
    }

    #[test]
    fn refs_aliases() {
        assert_eq!(parse("refs"), Command::Refs);
        assert_eq!(parse("ref"), Command::Refs);
        assert_eq!(parse("bib"), Command::Refs);
        assert_eq!(parse("bibliography"), Command::Refs);
        assert_eq!(parse("references"), Command::Refs);
    }

    #[test]
    fn info_and_diag_aliases() {
        assert_eq!(parse("info"), Command::Info);
        assert_eq!(parse("metadata"), Command::Info);
        assert_eq!(parse("diag"), Command::Diag);
        assert_eq!(parse("diagnostics"), Command::Diag);
    }

    /// Adversarial-input fuzz lite: bytes-as-strings shouldn't panic.
    /// Catches any future regex-meta / utf-8-edge bug in the parser.
    #[test]
    fn arbitrary_input_does_not_panic() {
        for s in &[
            "",
            "\0",
            "set\tnodark",
            ":::",
            "goto -1",
            "goto 999999999999999999999",
            "set dark dark dark dark",
            "naïve", // multi-byte
            "🦀 emoji",
            &"x".repeat(10_000),
        ] {
            // The contract is "doesn't panic"; we don't assert on the
            // resulting variant here.
            let _ = parse(s);
        }
    }
}
