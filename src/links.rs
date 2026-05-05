//! Vimium-style link follow.
//!
//! Enumerates a page's clickable links via pdfium and exposes a
//! simple `LinkOnPage` struct the UI can use for hint placement.
//! Two action types matter for a PDF reader:
//!   * Internal page jump (`GoToPage(page_idx)`)
//!   * External URL (`Url(String)`)
//! Anything else (form fill, JavaScript, named destination we can't
//! resolve, etc.) becomes `Other` so the UI can grey out the hint
//! or skip it entirely.

use pdfium_render::prelude::*;

use crate::highlight::Rect01;
use crate::pdf::PageMetrics;

#[derive(Debug, Clone)]
pub enum LinkAction {
    /// Internal jump within this document. Page index is 0-based.
    GoToPage(usize),
    /// External URL. Caller dispatches via `xdg-open` / `open` /
    /// platform-specific.
    Url(String),
    /// Some other action type we don't handle — caller may want to
    /// skip these from the hint set so the user doesn't try to
    /// activate something that does nothing.
    #[allow(dead_code)] // surfaced via match by callers
    Other,
}

/// One link region on a page, in normalised page coordinates so the
/// UI math (the same `norm_to_pixels` helpers used for highlights
/// and search hits) translates directly to terminal cells.
#[derive(Debug, Clone)]
pub struct LinkOnPage {
    pub rect: Rect01,
    pub action: LinkAction,
}

/// Read every link on `page` and return them as `LinkOnPage` rects.
///
/// Robust to pdfium failures on individual links: if a link's
/// bounds or action can't be read we silently skip it. Better to
/// show fewer hints than crash on a malformed link.
pub fn enumerate(page: &PdfPage<'_>, metrics: &PageMetrics) -> Vec<LinkOnPage> {
    let mut out = Vec::new();
    // pdfium-render exposes `page.links()` returning `PdfPageLinks`.
    // Iterate via .iter().
    let links = page.links();
    for link in links.iter() {
        // Rect in PDF point space (origin = bottom-left).
        let bounds = match link.rect() {
            Ok(b) => b,
            Err(_) => continue,
        };
        let rect = pdf_rect_to_norm(&bounds, metrics);

        // Action: either a destination (page jump) or a URI.
        let action = match link.action() {
            Some(a) => match resolve_action(&a) {
                Some(act) => act,
                None => LinkAction::Other,
            },
            None => {
                // No action at all — pdfium's "annotation only" link.
                // Skip rather than emit a useless hint.
                continue;
            }
        };
        // Drop Other (degenerate) actions so users don't see hints
        // that go nowhere.
        if matches!(action, LinkAction::Other) {
            continue;
        }
        out.push(LinkOnPage { rect, action });
    }
    out
}

fn resolve_action(action: &PdfAction<'_>) -> Option<LinkAction> {
    // pdfium-render normalises actions through `as_uri_action` /
    // `as_local_destination_action`. Try URI first (cheap hit for
    // arXiv / web links); fall back to internal destination.
    if let Some(uri) = action.as_uri_action() {
        if let Ok(u) = uri.uri() {
            return Some(LinkAction::Url(u));
        }
    }
    if let Some(dest_action) = action.as_local_destination_action() {
        if let Ok(dest) = dest_action.destination() {
            // Destination's page() is the absolute page index in the
            // doc. PdfDestination::page returns Result<PdfPageIndex>.
            if let Ok(page_index) = dest.page_index() {
                return Some(LinkAction::GoToPage(page_index as usize));
            }
        }
    }
    None
}

/// PDF point space (origin bottom-left, units = points) → normalised
/// 0..1 (origin top-left). Same math as search.rs::pdf_rect_to_norm.
fn pdf_rect_to_norm(r: &PdfRect, m: &PageMetrics) -> Rect01 {
    // Defend against malformed PDFs where rect coords or page metrics
    // come back NaN/Inf — `f32::clamp` panics if the bound is NaN, and
    // even when it doesn't, NaN propagates through arithmetic and
    // produces a hint Rect01 with NaN coords that pixel-mapper code
    // truncates to 0 (zero-size hint, silently swallowed).
    let raw_left = r.left().value;
    let raw_right = r.right().value;
    let raw_top = r.top().value;
    let raw_bottom = r.bottom().value;
    if !raw_left.is_finite()
        || !raw_right.is_finite()
        || !raw_top.is_finite()
        || !raw_bottom.is_finite()
        || !m.width_pts.is_finite()
        || !m.height_pts.is_finite()
    {
        return Rect01 {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
    }
    let w = m.width_pts.max(1.0);
    let h = m.height_pts.max(1.0);
    let left = raw_left.min(raw_right);
    let right = raw_left.max(raw_right);
    let top = raw_top.max(raw_bottom);
    let bottom = raw_top.min(raw_bottom);
    let x0 = (left / w).clamp(0.0, 1.0);
    let y0 = (1.0 - top / h).clamp(0.0, 1.0);
    let x1 = (right / w).clamp(0.0, 1.0);
    let y1 = (1.0 - bottom / h).clamp(0.0, 1.0);
    Rect01 {
        x: x0,
        y: y0,
        w: (x1 - x0).max(0.0),
        h: (y1 - y0).max(0.0),
    }
}

/// Hint-character pool. Excludes characters that look alike at small
/// terminal sizes (`l`/`1`/`I`, `o`/`0`) so the user doesn't squint.
/// Single chars first, then 2-char combos in the same alphabet —
/// gives 18 + 18² = 342 unique hints, plenty for any visible region.
pub const HINT_ALPHABET: &str = "abcdefghjkmnpqrstuvwxyz"; // 23 chars (drop ilo + ones that confuse with O/0)

/// Generate `n` distinct hint strings, shortest first. Greedy: use
/// 1-char hints until we'd exhaust them, then prefix-share 2-char
/// hints. Implementation matches the conventional vimium algorithm.
pub fn gen_hints(n: usize) -> Vec<String> {
    let alphabet: Vec<char> = HINT_ALPHABET.chars().collect();
    let a_len = alphabet.len();
    if n == 0 {
        return Vec::new();
    }
    if n <= a_len {
        return (0..n).map(|i| alphabet[i].to_string()).collect();
    }
    // Mixed: fill with 2-char hints starting with a prefix that
    // doesn't conflict with the single-char hints we keep. To keep
    // single-char hints unambiguous, reserve enough 2-char prefixes
    // up-front. Simple algorithm: use ALL combinations of (prefix,
    // tail) until we have enough, EXCEPT the prefix that would
    // shadow a single-char hint.
    //
    // Concretely: if the user types 'a', and 'a' is both a 1-char
    // hint AND a prefix of 'ab', we have to wait to see if there's
    // a second char. Vimium handles this by pre-empting: we never
    // emit a single-char hint whose char is a prefix of any 2-char
    // hint we also emit.
    //
    // So: reserve some chars purely as 1-char hints, others purely
    // as prefixes for 2-char hints. Optimal split when n > a_len:
    // use (n - a_len) // (a_len - 1) prefixes for 2-char hints,
    // remaining chars stay 1-char.
    let mut hints = Vec::with_capacity(n);
    // How many 2-char hints we need to cover after we use some
    // 1-char hints. With p prefixes, we get p * a_len 2-char hints.
    // We need `n - (a_len - p)` two-char hints: solve for p.
    let mut p = 1;
    while (a_len - p) + p * a_len < n && p < a_len {
        p += 1;
    }
    // Use the LAST `p` chars as 2-char prefixes; the first
    // `a_len - p` as standalone 1-char hints. Visually: the
    // common-sequence chars like 'a' get shorter codes.
    let single_count = a_len - p;
    for &c in alphabet.iter().take(single_count) {
        hints.push(c.to_string());
        if hints.len() == n {
            return hints;
        }
    }
    'outer: for &prefix in alphabet.iter().take(a_len).skip(single_count) {
        for &tail in &alphabet {
            let mut s = String::with_capacity(2);
            s.push(prefix);
            s.push(tail);
            hints.push(s);
            if hints.len() == n {
                break 'outer;
            }
        }
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gen_hints_zero_returns_empty() {
        assert!(gen_hints(0).is_empty());
    }

    #[test]
    fn gen_hints_under_alphabet_uses_single_chars() {
        let h = gen_hints(5);
        assert_eq!(h.len(), 5);
        for s in &h {
            assert_eq!(s.chars().count(), 1, "single-char expected: {s}");
        }
        // All distinct.
        let dedup: std::collections::HashSet<&String> = h.iter().collect();
        assert_eq!(dedup.len(), 5);
    }

    #[test]
    fn gen_hints_over_alphabet_mixes_lengths_no_prefix_conflicts() {
        let n = 50;
        let h = gen_hints(n);
        assert_eq!(h.len(), n);
        // A single-char hint must not be a prefix of any other hint —
        // otherwise the user wouldn't know whether typing `a` means
        // hint `a` or "wait for the second char of `ab`".
        let single_chars: std::collections::HashSet<char> = h
            .iter()
            .filter(|s| s.chars().count() == 1)
            .map(|s| s.chars().next().unwrap())
            .collect();
        for s in &h {
            if s.chars().count() > 1 {
                let prefix = s.chars().next().unwrap();
                assert!(
                    !single_chars.contains(&prefix),
                    "hint {s} prefixed by single-char hint {prefix}"
                );
            }
        }
    }

    #[test]
    fn gen_hints_large_count_fits_alphabet_squared() {
        let alphabet_len = HINT_ALPHABET.chars().count();
        // 2-char ceiling: a_len^2 — plus the unconflicting 1-char
        // headroom. For our ~23-char alphabet that's well over 500.
        let h = gen_hints(alphabet_len * (alphabet_len - 1));
        assert!(h.len() >= alphabet_len);
    }

    #[test]
    fn hint_alphabet_excludes_lookalikes() {
        for ch in ['l', 'i', 'o', '0', '1'] {
            assert!(
                !HINT_ALPHABET.contains(ch),
                "{ch:?} must be excluded from hint alphabet"
            );
        }
    }
}
