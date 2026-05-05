//! Document outline (table of contents) — eager-loaded, flattened
//! into a depth-tagged list, optionally filterable by substring.
//!
//! Pdfium gives us a tree of `PdfBookmark`s with `title()`,
//! `destination()`, `first_child()`, `next_sibling()`. We flatten
//! depth-first so the panel can render with simple j/k navigation —
//! tree expand/collapse is more complexity than the average user
//! needs, and 99% of papers/books have ≤ 30 entries.

use anyhow::Result;
use pdfium_render::prelude::*;

#[derive(Debug, Clone)]
pub struct OutlineEntry {
    pub title: String,
    /// Pre-lowercased title cached at load time. Filtering used to
    /// re-lowercase + re-collect into a `Vec<char>` for every entry
    /// on every keystroke (O(N·M) per char on a 50k-entry outline);
    /// caching the chars here makes filter-as-you-type instant.
    pub lc_title: Vec<char>,
    pub depth: u8,
    /// Resolved page index (0-based). `None` when the bookmark has
    /// no destination, or the destination doesn't resolve to a page
    /// in this document — those entries still render (greyed) so the
    /// document structure is visible, but Enter is a no-op on them.
    pub page: Option<usize>,
}

/// Cap how many entries a single document can contribute. Bookmark
/// trees in real PDFs are tens to a few hundred deep; a value in the
/// tens of thousands suggests either an unusually massive reference
/// work or a malicious file. Either way, refusing to grow the panel
/// past this cap protects memory and the sibling-loop runtime.
const MAX_OUTLINE_ENTRIES: usize = 50_000;

/// Cap on bookmark tree depth. Real outlines never exceed ~10; the
/// indent rendering already caps display at 6. The hard cap here
/// protects the recursive `walk_children` against pathological PDFs
/// that nest first_child arbitrarily deep.
const MAX_OUTLINE_DEPTH: u8 = 64;

/// Walk the bookmark tree depth-first, producing a flat, depth-
/// tagged list. Returns an empty Vec for documents with no outline.
pub fn load(document: &PdfDocument<'_>) -> Result<Vec<OutlineEntry>> {
    let mut out = Vec::new();
    let bookmarks = document.bookmarks();
    if let Some(root) = bookmarks.root() {
        // The "root" bookmark has no title of its own; iterate its
        // children. Some PDFs nest everything under a top-level
        // bookmark (which *does* have a title) — handled the same
        // way because walk() emits a title-bearing root then
        // recurses into its children.
        let mut step_budget: usize = MAX_OUTLINE_STEPS;
        walk(root, 0, &mut out, &mut step_budget);
    }
    Ok(out)
}

/// Hard cap on bookmark-tree walk steps. Protects against malicious
/// PDFs whose `next_sibling()` returns a previously-seen node, creating
/// an infinite loop bounded only by `MAX_OUTLINE_ENTRIES` per push but
/// otherwise burning CPU forever after the entry vec fills. 500k is
/// 10× the entry cap so a worst-case-shape legitimate doc still
/// traverses fully.
const MAX_OUTLINE_STEPS: usize = 500_000;

fn walk(start: PdfBookmark<'_>, depth: u8, out: &mut Vec<OutlineEntry>, steps: &mut usize) {
    // Iterate the sibling chain instead of recursing into it. PDFs
    // with very long sibling chains (10k+, occasionally seen in
    // adversarial files or auto-generated reference indexes) would
    // otherwise blow the stack. Children stay recursive — depths are
    // small and capped by MAX_OUTLINE_DEPTH.
    let mut cur = Some(start);
    while let Some(b) = cur {
        if *steps == 0 {
            return;
        }
        *steps -= 1;
        if out.len() >= MAX_OUTLINE_ENTRIES {
            return;
        }
        if let Some(title) = b.title() {
            // Sanitize before storing — outline titles come straight
            // from the PDF and a malicious file can embed CSI escapes
            // or bidi-overrides that flip subsequent rows of the TOC
            // panel and spoof neighbouring entry text. Cap raw-title
            // length too: a 50k-entry outline with megabyte titles
            // would otherwise allocate gigabytes before render-time
            // truncation kicks in. 4 KiB is far above any real title.
            const MAX_RAW_TITLE_LEN: usize = 4096;
            let raw = if title.len() > MAX_RAW_TITLE_LEN {
                let mut end = MAX_RAW_TITLE_LEN;
                while end > 0 && !title.is_char_boundary(end) {
                    end -= 1;
                }
                &title[..end]
            } else {
                title.as_str()
            };
            let safe_title = crate::term_safe::safe_for_stderr(raw);
            let page = b
                .destination()
                .and_then(|d| d.page_index().ok())
                .map(|p| p as usize);
            let lc_title: Vec<char> = safe_title.to_lowercase().chars().collect();
            out.push(OutlineEntry {
                title: safe_title,
                lc_title,
                depth,
                page,
            });
        }
        if depth < MAX_OUTLINE_DEPTH {
            if let Some(child) = b.first_child() {
                walk(child, depth.saturating_add(1), out, steps);
            }
        }
        cur = b.next_sibling();
    }
}

/// Filter entries by case-insensitive subsequence match. An empty
/// query returns every index, in order. Returns indices into
/// `entries` rather than cloned entries so the panel can render
/// with `entries[i]` and selection state stays index-based.
pub fn fuzzy_filter(entries: &[OutlineEntry], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..entries.len()).collect();
    }
    let needle: Vec<char> = query.to_lowercase().chars().collect();
    entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            if is_subsequence(&needle, &e.lc_title) {
                Some(i)
            } else {
                None
            }
        })
        .collect()
}

fn is_subsequence(needle: &[char], hay: &[char]) -> bool {
    let mut ni = 0;
    for &c in hay {
        if ni < needle.len() && needle[ni] == c {
            ni += 1;
        }
    }
    ni == needle.len()
}

/// Format an entry for the panel. Caps indent at depth 6 so deeply
/// nested outlines don't push the title off the right edge of a
/// narrow panel; deeper entries are still listed, just not further
/// indented.
pub fn render_line(entry: &OutlineEntry, max_width: usize) -> String {
    let indent = "  ".repeat(entry.depth.min(6) as usize);
    let page_suffix = match entry.page {
        Some(p) => format!(" p.{}", p + 1),
        None => "  —".to_string(),
    };
    let suffix_len = page_suffix.chars().count();
    let title_budget = max_width
        .saturating_sub(indent.chars().count())
        .saturating_sub(suffix_len)
        .saturating_sub(1);
    let title = truncate(&entry.title, title_budget);
    format!("{indent}{title} {page_suffix}")
}

fn truncate(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_string();
    }
    if max_chars <= 1 {
        return "…".to_string();
    }
    let kept: String = chars[..max_chars - 1].iter().collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str, depth: u8, page: Option<usize>) -> OutlineEntry {
        OutlineEntry {
            title: title.into(),
            lc_title: title.to_lowercase().chars().collect(),
            depth,
            page,
        }
    }

    #[test]
    fn fuzzy_filter_empty_query_returns_all() {
        let v = vec![entry("a", 0, None), entry("b", 0, None)];
        assert_eq!(fuzzy_filter(&v, ""), vec![0, 1]);
    }

    #[test]
    fn fuzzy_filter_subsequence_match_is_case_insensitive() {
        let v = vec![
            entry("Introduction", 0, Some(0)),
            entry("Methods", 0, Some(5)),
            entry("Results and Discussion", 0, Some(20)),
        ];
        // "rd" is a subsequence of both "Introduction" (I-n-t-R-o-D)
        // and "Results and Discussion" — by design. Filter is loose
        // matching, like fzf's default scoring.
        assert_eq!(fuzzy_filter(&v, "rd"), vec![0, 2]);
        // "in" matches "Introduction" and "Discussion".
        assert_eq!(fuzzy_filter(&v, "in"), vec![0, 2]);
        // "method" matches Methods only.
        assert_eq!(fuzzy_filter(&v, "method"), vec![1]);
    }

    #[test]
    fn fuzzy_filter_no_match_returns_empty() {
        let v = vec![entry("Hello", 0, None)];
        assert_eq!(fuzzy_filter(&v, "xyz"), Vec::<usize>::new());
    }

    #[test]
    fn render_line_truncates_long_titles() {
        let e = entry(&"x".repeat(100), 0, Some(9));
        let s = render_line(&e, 30);
        assert!(
            s.chars().count() <= 30,
            "got {} chars: {:?}",
            s.chars().count(),
            s
        );
        assert!(s.contains('…'));
        assert!(s.ends_with("p.10"));
    }

    #[test]
    fn render_line_indents_by_depth() {
        let e = entry("section", 2, Some(0));
        let s = render_line(&e, 80);
        assert!(s.starts_with("    "), "expected 4-space indent, got {s:?}");
    }

    #[test]
    fn render_line_caps_indent_at_six() {
        let e = entry("deep", 20, Some(0));
        let s = render_line(&e, 80);
        // 6 levels × 2 spaces = 12 leading spaces.
        let leading = s.chars().take_while(|c| *c == ' ').count();
        assert_eq!(leading, 12);
    }

    #[test]
    fn render_line_marks_unresolved_destination() {
        let e = entry("broken", 0, None);
        let s = render_line(&e, 80);
        assert!(s.contains("—"), "got {s:?}");
    }

    #[test]
    fn truncate_at_max_one_returns_just_ellipsis() {
        assert_eq!(truncate("hello", 1), "…");
    }
}
