//! Per-page text geometry: char positions, line clustering, word
//! boundaries. Built lazily from pdfium's `PdfPageText` once per page,
//! cached as long as the page is kept alive.
//!
//! This is the data layer for vim-style caret motions (`hjkl`, `w`,
//! `b`, `e`, `0`, `$`, `f<c>`, …). Selection lives as a pair of
//! `Caret { page, char_idx }`s; rendering goes through pdfium's
//! `segments_subset` so a multi-line selection is drawn as one rect
//! per visual line — the same "3-band" model okular and mupdf use.

use std::collections::HashMap;

use anyhow::{Context, Result};
use pdfium_render::prelude::*;

use crate::highlight::Rect01;
use crate::pdf::PageMetrics;

/// Position of a single caret in the document: a page index plus a
/// char index inside that page's stream (`PageText.chars[idx]`).
/// Carets are addresses into a *visible* page's text — anything off-
/// page is `None` rather than encoded here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caret {
    pub page: usize,
    pub idx: usize,
}

/// Vim-style visual-mode selection: anchor stays where Visual was
/// entered; head follows motion / drag. The selection covers
/// `min(anchor, head) ..= max(anchor, head)` in document order
/// (page-major, then char-index-major).
#[derive(Debug, Clone, Copy)]
pub struct TextSelection {
    pub anchor: Caret,
    pub head: Caret,
    /// Selection flavour. Phase 1 only constructs `Charwise`; the
    /// other variants are wired in Phase 4 (`V` and `<C-v>`).
    #[allow(dead_code)]
    pub mode: SelMode,
}

/// vim's three visual flavours. `Charwise` is `v`, `Linewise` is `V`,
/// `Blockwise` is `<C-v>`. Phase 1 wires only `Charwise`; the others
/// land in Phase 4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SelMode {
    Charwise,
    Linewise,
    Blockwise,
}

impl TextSelection {
    pub fn new(at: Caret) -> Self {
        Self { anchor: at, head: at, mode: SelMode::Charwise }
    }

    /// Lower (earlier in reading order) and upper carets.
    pub fn ordered(&self) -> (Caret, Caret) {
        if (self.anchor.page, self.anchor.idx) <= (self.head.page, self.head.idx) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// A single character on a page: its index inside pdfium's text run,
/// the unicode codepoint (`None` for invalid / generated chars), the
/// loose-bounds bbox in normalised top-left page coords, and the
/// visual line it belongs to. The `origin_x` is kept verbatim
/// (normalised 0..1) so vertical motion `j`/`k` can re-pin the caret
/// to the same column in the next/previous line.
#[derive(Debug, Clone)]
pub struct CharCell {
    /// Pdfium's stream-order index. Phase 4 rect-mode selection will
    /// use this to ask pdfium for `segments_subset(start, count)`
    /// rect lists; today's geometric path doesn't need it.
    #[allow(dead_code)]
    pub idx: usize,
    pub ch: Option<char>,
    pub bbox: Rect01,
    pub line: usize,
    pub origin_x: f32,
    pub is_generated: bool,
}

/// One visual line on a page: char-index range plus per-line word
/// boundaries (indices into `PageText.chars`). Word boundaries use
/// a cheap unicode-aware whitespace heuristic — sufficient for
/// `w`/`b`/`e` motions; can graduate to `unicode-segmentation` if
/// users hit edge cases with CJK or punctuation runs.
#[derive(Debug, Clone)]
pub struct LineSpan {
    pub y_top: f32,
    pub y_bot: f32,
    pub start_idx: usize,
    pub end_idx: usize,
    pub word_starts: Vec<usize>,
}

/// Whole-page geometry.
#[derive(Debug, Clone)]
pub struct PageText {
    /// Page index this layout belongs to. Used by upcoming phases
    /// (cross-page caret motion) and for diagnostic logging.
    #[allow(dead_code)]
    pub page_idx: usize,
    pub chars: Vec<CharCell>,
    pub lines: Vec<LineSpan>,
    /// Page dimensions in PDF points — kept so a future "selection
    /// → PDF coords" pass can convert without a `PageMetrics` round-trip.
    #[allow(dead_code)]
    pub width_pts: f32,
    #[allow(dead_code)]
    pub height_pts: f32,
}

impl PageText {
    /// Eagerly walk every char on the page; build `CharCell`s in
    /// stream order, then cluster into visual lines by Y-band.
    pub fn load(
        document: &PdfDocument<'_>,
        page_idx: usize,
        metrics: &PageMetrics,
    ) -> Result<Self> {
        let pages = document.pages();
        let page = pages
            .get(page_idx as i32)
            .with_context(|| format!("opening page {} for text layout", page_idx + 1))?;
        let text = page
            .text()
            .with_context(|| format!("loading text page {}", page_idx + 1))?;

        let w = metrics.width_pts.max(1.0);
        let h = metrics.height_pts.max(1.0);

        let chars_collection = text.chars();
        // Pre-size to the pdfium-reported char count so the per-char
        // push doesn't grow the Vec mid-build. A typical PDF page has
        // 500-3000 chars; growing from 0 takes ~9 reallocs at small N
        // and a final 32-128 KB malloc at large N.
        let mut chars: Vec<CharCell> = Vec::with_capacity(chars_collection.len() as usize);
        for ch in chars_collection.iter() {
            let bbox = match ch.loose_bounds() {
                Ok(r) => r,
                Err(_) => continue,
            };
            // Y flip: pdfium PdfRect is bottom-left; we want top-left.
            let x = (bbox.left().value / w).clamp(0.0, 1.0);
            let right = (bbox.right().value / w).clamp(0.0, 1.0);
            let top = ((h - bbox.top().value) / h).clamp(0.0, 1.0);
            let bot = ((h - bbox.bottom().value) / h).clamp(0.0, 1.0);
            chars.push(CharCell {
                idx: ch.index() as usize,
                ch: ch.unicode_char(),
                bbox: Rect01 {
                    x,
                    y: top,
                    w: (right - x).max(0.0),
                    h: (bot - top).max(0.0),
                },
                line: 0, // filled in by line clustering below
                origin_x: x,
                is_generated: ch.is_generated().unwrap_or(false),
            });
        }

        let lines = cluster_lines(&mut chars);

        Ok(Self {
            page_idx,
            chars,
            lines,
            width_pts: w,
            height_pts: h,
        })
    }

    /// Find the index of the char nearest `(nx, ny)` in normalised
    /// page coords, or `None` if the page has no chars at all.
    /// Prefers exact-hit (point inside the bbox) but falls back to
    /// the nearest-by-centre when the click lands in whitespace.
    pub fn char_at_point(&self, nx: f32, ny: f32) -> Option<usize> {
        if self.chars.is_empty() {
            return None;
        }
        // Exact hit first.
        for (i, c) in self.chars.iter().enumerate() {
            if nx >= c.bbox.x
                && nx <= c.bbox.x + c.bbox.w
                && ny >= c.bbox.y
                && ny <= c.bbox.y + c.bbox.h
            {
                return Some(i);
            }
        }
        // Snap to nearest-by-center on the closest line.
        let mut best = (0usize, f32::MAX);
        for (i, c) in self.chars.iter().enumerate() {
            let cx = c.bbox.x + c.bbox.w * 0.5;
            let cy = c.bbox.y + c.bbox.h * 0.5;
            let dy = (cy - ny).abs();
            // Heavily penalise off-line distance — clicking in a gap
            // between lines should snap to the nearer line, not jump
            // far away in X.
            let d = dy * 4.0 + (cx - nx).abs();
            if d < best.1 {
                best = (i, d);
            }
        }
        Some(best.0)
    }

    /// Line index containing `char_idx`, if any. Generated/space
    /// chars carry the `line` of the line they belong to (see
    /// `cluster_lines`).
    pub fn line_of(&self, char_idx: usize) -> Option<usize> {
        self.chars.get(char_idx).map(|c| c.line)
    }

    /// First char index on `line`. `None` for empty/missing lines.
    /// Used by Phase 3's `0` / `^` motions.
    #[allow(dead_code)]
    pub fn line_start(&self, line: usize) -> Option<usize> {
        self.lines.get(line).map(|l| l.start_idx)
    }

    /// Last char index on `line` (inclusive). Used by Phase 3's `$`.
    #[allow(dead_code)]
    pub fn line_end(&self, line: usize) -> Option<usize> {
        self.lines.get(line).map(|l| l.end_idx)
    }

    /// First non-whitespace char on `line` (vim's `^`). Falls back
    /// to `line_start` if the whole line is whitespace.
    pub fn line_first_non_blank(&self, line: usize) -> Option<usize> {
        let span = self.lines.get(line)?;
        for i in span.start_idx..=span.end_idx {
            let c = &self.chars[i];
            if c.line != line {
                continue;
            }
            let blank = c.ch.map(|ch| ch.is_whitespace()).unwrap_or(true);
            if !blank {
                return Some(i);
            }
        }
        Some(span.start_idx)
    }

    /// Index of the next word-start at or after `from`, walking
    /// forward across lines if necessary. Returns the last char on
    /// the page if no further word-start exists.
    ///
    /// Walks through lines until it finds one with a word_start —
    /// previously this stopped after the very next line, so a `w`
    /// motion across two consecutive blank/whitespace-only lines
    /// would land on the whitespace instead of the actual next word
    /// (vim's `w` keeps walking).
    pub fn next_word_start(&self, from: usize) -> usize {
        let start_line = self.line_of(from).unwrap_or(0);
        // First, look on the current line for any word-start past `from`.
        if let Some(span) = self.lines.get(start_line) {
            for &ws in &span.word_starts {
                if ws > from {
                    return ws;
                }
            }
        }
        // Otherwise, walk subsequent lines until we hit one that
        // genuinely starts a word.
        let mut line_idx = start_line + 1;
        while let Some(span) = self.lines.get(line_idx) {
            if let Some(&ws) = span.word_starts.first() {
                return ws;
            }
            line_idx += 1;
        }
        self.chars.len().saturating_sub(1)
    }

    /// Index of the previous word-start at or before `from`. Returns
    /// `0` if none.
    ///
    /// Walks through preceding lines until one with an actual
    /// word_start is found, mirroring `next_word_start`. The earlier
    /// version only inspected the immediately previous line, so a
    /// `b` motion across two whitespace-only lines could land on
    /// pure whitespace instead of the closest real word.
    pub fn prev_word_start(&self, from: usize) -> usize {
        let start_line = self.line_of(from).unwrap_or(0);
        if let Some(span) = self.lines.get(start_line) {
            for &ws in span.word_starts.iter().rev() {
                if ws < from {
                    return ws;
                }
            }
        }
        if start_line == 0 {
            return 0;
        }
        let mut line_idx = start_line - 1;
        loop {
            if let Some(span) = self.lines.get(line_idx) {
                if let Some(&ws) = span.word_starts.last() {
                    return ws;
                }
            }
            if line_idx == 0 {
                return 0;
            }
            line_idx -= 1;
        }
    }

    /// Index of the end of the word containing or after `from` —
    /// vim's `e`. Returns the last char of the page if `from` is
    /// already at the doc's last word's end.
    pub fn end_of_word(&self, from: usize) -> usize {
        let n = self.chars.len();
        if n == 0 {
            return 0;
        }
        let mut i = from.min(n - 1);
        // If on whitespace, advance into the next word first.
        let is_blank = |idx: usize| {
            self.chars[idx]
                .ch
                .map(|ch| ch.is_whitespace())
                .unwrap_or(true)
        };
        while i + 1 < n && is_blank(i) {
            i += 1;
        }
        // Walk forward while still on a word char; stop on the last
        // word char before whitespace or end.
        while i + 1 < n && !is_blank(i + 1) {
            i += 1;
        }
        i
    }

    /// Find the next occurrence of `target` (case-sensitive) on the
    /// same line as `from`, strictly after `from`. Vim's `f<c>`.
    ///
    /// Span [start_idx, end_idx] is in stream order; pdfium can
    /// interleave chars from neighbouring lines (e.g. footnote chars
    /// sandwiched mid-paragraph), so the linear scan must filter
    /// `c.line == line` or `f<c>` could land off the user's line.
    pub fn find_char_in_line(&self, from: usize, target: char) -> Option<usize> {
        let line = self.line_of(from)?;
        let span = self.lines.get(line)?;
        ((from + 1)..=span.end_idx).find(|&i| {
            let c = &self.chars[i];
            c.line == line && c.ch == Some(target)
        })
    }

    /// Vim's `F<c>` — previous occurrence on the same line.
    pub fn rfind_char_in_line(&self, from: usize, target: char) -> Option<usize> {
        let line = self.line_of(from)?;
        let span = self.lines.get(line)?;
        (span.start_idx..from).rev().find(|&i| {
            let c = &self.chars[i];
            c.line == line && c.ch == Some(target)
        })
    }

    /// Word range surrounding `from` — `[start, end]` inclusive,
    /// covering the contiguous run of non-whitespace chars. Returns
    /// `None` if `from` lies on whitespace and there's no adjacent
    /// word (vim's `iw` used on whitespace selects the whitespace
    /// run instead; we keep the simpler "snap to nearest word"
    /// behavior for now).
    pub fn word_around(&self, from: usize) -> Option<(usize, usize)> {
        let n = self.chars.len();
        if n == 0 {
            return None;
        }
        let from = from.min(n - 1);
        let is_blank = |idx: usize| {
            self.chars[idx]
                .ch
                .map(|ch| ch.is_whitespace())
                .unwrap_or(true)
        };
        // If on a blank, snap forward to the next word (matches vim
        // when `iw` lands on whitespace under cursor).
        let mut anchor = from;
        if is_blank(anchor) {
            while anchor + 1 < n && is_blank(anchor) {
                anchor += 1;
            }
            if is_blank(anchor) {
                return None;
            }
        }
        let mut start = anchor;
        while start > 0 && !is_blank(start - 1) {
            start -= 1;
        }
        let mut end = anchor;
        while end + 1 < n && !is_blank(end + 1) {
            end += 1;
        }
        Some((start, end))
    }

    /// Sentence range around `from`. Sentences end at `. ! ?` followed
    /// by whitespace or end-of-line. Sufficient for prose; doesn't
    /// try to handle abbreviations. Returns inclusive `[start, end]`.
    pub fn sentence_around(&self, from: usize) -> Option<(usize, usize)> {
        let n = self.chars.len();
        if n == 0 {
            return None;
        }
        let from = from.min(n - 1);
        let is_terminator = |idx: usize| {
            matches!(self.chars[idx].ch, Some('.') | Some('!') | Some('?'))
        };
        // Walk back to start: previous terminator (skipping past it
        // and any following whitespace).
        let mut start = from;
        while start > 0 {
            if is_terminator(start - 1) {
                break;
            }
            start -= 1;
        }
        // Skip any leading whitespace.
        while start < n
            && self.chars[start]
                .ch
                .map(|ch| ch.is_whitespace())
                .unwrap_or(true)
        {
            start += 1;
            if start >= n {
                return None;
            }
        }
        // Walk forward to end: next terminator (inclusive).
        let mut end = from;
        while end + 1 < n && !is_terminator(end) {
            end += 1;
        }
        Some((start, end))
    }

    /// Paragraph range around `from`. Paragraph boundaries are
    /// detected from gaps between visual lines (Y gap > 1.6× the
    /// median line height) — a simple and reliable heuristic for
    /// body text. Returns inclusive `[start, end]`.
    pub fn paragraph_around(&self, from: usize) -> Option<(usize, usize)> {
        if self.chars.is_empty() || self.lines.is_empty() {
            return None;
        }
        let from = from.min(self.chars.len() - 1);
        let cur_line = self.line_of(from)?;
        // Median line height as gap threshold.
        let mut heights: Vec<f32> = self
            .lines
            .iter()
            .map(|l| (l.y_bot - l.y_top).max(1e-6))
            .collect();
        heights.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med = heights[heights.len() / 2].max(1e-4);
        let gap_threshold = med * 1.6;

        let mut start_line = cur_line;
        while start_line > 0 {
            let prev = &self.lines[start_line - 1];
            let here = &self.lines[start_line];
            if here.y_top - prev.y_bot > gap_threshold {
                break;
            }
            start_line -= 1;
        }
        let mut end_line = cur_line;
        while end_line + 1 < self.lines.len() {
            let here = &self.lines[end_line];
            let next = &self.lines[end_line + 1];
            if next.y_top - here.y_bot > gap_threshold {
                break;
            }
            end_line += 1;
        }
        Some((self.lines[start_line].start_idx, self.lines[end_line].end_idx))
    }

    /// Per-visual-line rectangles covering the inclusive char range
    /// `[start, end]`. Implements the textbook 3-band model: tail of
    /// the start line, full bands of inner lines, head of the end
    /// line. Returns one rect per line, in top-to-bottom order.
    pub fn range_to_rects(&self, start: usize, end: usize) -> Vec<Rect01> {
        if self.chars.is_empty() {
            return Vec::new();
        }
        let (lo, hi) = if start <= end { (start, end) } else { (end, start) };
        let lo = lo.min(self.chars.len() - 1);
        let hi = hi.min(self.chars.len() - 1);
        let line_lo = self.chars[lo].line;
        let line_hi = self.chars[hi].line;

        // Upper bound: one rect per line in [line_lo, line_hi]. The
        // bake-selection path calls this on every Visual-mode keystroke
        // — pre-sizing avoids a growth realloc on multi-line spans.
        let mut out: Vec<Rect01> = Vec::with_capacity(line_hi.saturating_sub(line_lo) + 1);
        for line_idx in line_lo..=line_hi {
            let Some(span) = self.lines.get(line_idx) else { continue };
            // Determine the char range covering THIS line within the
            // selection — the start of the selection on the first
            // line, the end of the selection on the last line, and
            // the full line's width on intermediate lines.
            let first = if line_idx == line_lo {
                lo
            } else {
                span.start_idx
            };
            let last = if line_idx == line_hi {
                hi
            } else {
                span.end_idx
            };
            // Compute the line rect from the bbox union of the
            // selected chars on that line. Skip lines whose chars
            // have zero width (empty/blank).
            let mut rect: Option<Rect01> = None;
            for c in &self.chars[first..=last.min(self.chars.len() - 1)] {
                if c.line != line_idx {
                    continue;
                }
                if c.bbox.w < 1e-6 || c.bbox.h < 1e-6 {
                    continue;
                }
                rect = Some(match rect {
                    Some(prev) => union_rect(prev, c.bbox),
                    None => c.bbox,
                });
            }
            if let Some(r) = rect {
                out.push(r);
            }
        }
        out
    }

    /// Extract text for the inclusive char range as a string.
    /// Inserts a `\n` between visual lines so paragraph breaks are
    /// preserved in the clipboard payload.
    pub fn extract(&self, start: usize, end: usize) -> String {
        if self.chars.is_empty() {
            return String::new();
        }
        let (lo, hi) = if start <= end { (start, end) } else { (end, start) };
        let lo = lo.min(self.chars.len() - 1);
        let hi = hi.min(self.chars.len() - 1);
        // Upper bound: each char contributes 1-4 UTF-8 bytes plus
        // possibly a newline. Average is closer to 1 byte, so size to
        // (hi - lo + 1) bytes — typical paragraph yanks fit without
        // reallocation, the rare wide-codepoint or many-line case grows
        // a single doubling step.
        let mut out = String::with_capacity(hi.saturating_sub(lo) + 1);
        let mut last_line = self.chars[lo].line;
        for c in &self.chars[lo..=hi] {
            if c.is_generated {
                continue;
            }
            if c.line != last_line {
                out.push('\n');
                last_line = c.line;
            }
            if let Some(ch) = c.ch {
                out.push(ch);
            }
        }
        out
    }
}

/// Cluster `chars` (in pdfium stream order) into visual lines by Y
/// band. Mutates each `CharCell::line` to point at its line index.
/// Returns `LineSpan`s in top-to-bottom order.
///
/// The clustering uses a tolerance proportional to the median char
/// height: a char joins the current line if its Y centre is within
/// half a line-height of the running mean Y centre. Stream order is
/// usually close to reading order for body text — multi-column or
/// tabular layouts may need a smarter pass later.
fn cluster_lines(chars: &mut [CharCell]) -> Vec<LineSpan> {
    if chars.is_empty() {
        return Vec::new();
    }

    // Build (idx, y_center, h) triples and sort by Y so we walk
    // top-to-bottom regardless of pdfium's stream order.
    let mut order: Vec<(usize, f32, f32)> = chars
        .iter()
        .enumerate()
        .map(|(i, c)| (i, c.bbox.y + c.bbox.h * 0.5, c.bbox.h.max(1e-6)))
        .collect();
    order.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    // Median height as the line-band tolerance.
    let mut heights: Vec<f32> = order.iter().map(|t| t.2).collect();
    heights.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let med_h = heights[heights.len() / 2].max(1e-4);
    let tol = med_h * 0.55;

    // First pass: walk in Y order, group chars into lines.
    let mut line_id: usize = 0;
    let mut line_y_sum = 0.0f32;
    let mut line_count = 0u32;
    let mut char_lines: Vec<usize> = vec![0; chars.len()];
    let mut last_centre = order[0].1;

    for (i, y, _h) in order.iter() {
        let mean = if line_count > 0 {
            line_y_sum / line_count as f32
        } else {
            *y
        };
        if (y - mean).abs() > tol && line_count > 0 {
            line_id += 1;
            line_y_sum = 0.0;
            line_count = 0;
        }
        line_y_sum += y;
        line_count += 1;
        last_centre = *y;
        char_lines[*i] = line_id;
    }
    let _ = last_centre;

    // Apply line ids back to chars.
    for (i, c) in chars.iter_mut().enumerate() {
        c.line = char_lines[i];
    }

    // Second pass: build LineSpan ranges. We need stream-order
    // contiguity to use start_idx..=end_idx as a range, so we sort
    // chars within each line by stream idx and emit min/max.
    let line_count_total = line_id + 1;
    let mut spans: Vec<LineSpan> = (0..line_count_total)
        .map(|_| LineSpan {
            y_top: f32::MAX,
            y_bot: 0.0,
            start_idx: usize::MAX,
            end_idx: 0,
            word_starts: Vec::new(),
        })
        .collect();
    for (i, c) in chars.iter().enumerate() {
        let s = &mut spans[c.line];
        if c.bbox.y < s.y_top {
            s.y_top = c.bbox.y;
        }
        let bot = c.bbox.y + c.bbox.h;
        if bot > s.y_bot {
            s.y_bot = bot;
        }
        if i < s.start_idx {
            s.start_idx = i;
        }
        if i > s.end_idx {
            s.end_idx = i;
        }
    }

    // Third pass: per-line word boundaries via simple Unicode
    // whitespace transitions over the chars in stream order on that
    // line. A char is a "word start" if it's a non-space and the
    // previous char on the same line was a space (or this is the
    // first char on the line).
    for span in spans.iter_mut() {
        let mut prev_was_space = true;
        for i in span.start_idx..=span.end_idx {
            let c = &chars[i];
            if c.line != chars[span.start_idx].line {
                continue;
            }
            let is_space = c
                .ch
                .map(|ch| ch.is_whitespace())
                .unwrap_or(true);
            if !is_space && prev_was_space {
                span.word_starts.push(i);
            }
            prev_was_space = is_space;
        }
    }

    spans
}

fn union_rect(a: Rect01, b: Rect01) -> Rect01 {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.w).max(b.x + b.w);
    let y1 = (a.y + a.h).max(b.y + b.h);
    Rect01 {
        x: x0,
        y: y0,
        w: (x1 - x0).max(0.0),
        h: (y1 - y0).max(0.0),
    }
}

/// LRU-style cache: holds `PageText` for any page the user has
/// touched while in select mode. Drop entries via `evict_far` when
/// the visible window moves so memory stays bounded.
#[derive(Default)]
pub struct TextCache {
    map: HashMap<usize, PageText>,
}

impl TextCache {
    pub fn get_or_load(
        &mut self,
        document: &PdfDocument<'_>,
        page_idx: usize,
        metrics: &PageMetrics,
    ) -> Result<&PageText> {
        // Entry-API match avoids the previous double-lookup pattern
        // (contains_key + insert + get = 3 hashes); this does at
        // most one. Distinct branches because `Entry::or_insert_with`
        // can't propagate the `?` from the fallible loader.
        use std::collections::hash_map::Entry;
        match self.map.entry(page_idx) {
            Entry::Occupied(e) => Ok(&*e.into_mut()),
            Entry::Vacant(e) => {
                let pt = PageText::load(document, page_idx, metrics)?;
                Ok(&*e.insert(pt))
            }
        }
    }

    pub fn get(&self, page_idx: usize) -> Option<&PageText> {
        self.map.get(&page_idx)
    }

    pub fn retain<F: FnMut(usize) -> bool>(&mut self, mut keep: F) {
        self.map.retain(|&k, _| keep(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(idx: usize, x: f32, y: f32, w: f32, h: f32, ch: char) -> CharCell {
        CharCell {
            idx,
            ch: Some(ch),
            bbox: Rect01 { x, y, w, h },
            line: 0,
            origin_x: x,
            is_generated: false,
        }
    }

    fn page_with(chars: Vec<CharCell>) -> PageText {
        let mut chars = chars;
        let lines = cluster_lines(&mut chars);
        PageText {
            page_idx: 0,
            chars,
            lines,
            width_pts: 100.0,
            height_pts: 200.0,
        }
    }

    #[test]
    fn clusters_two_visual_lines() {
        // Three chars on line A (y=0.1), three on line B (y=0.3).
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, 'a'),
            cell(1, 0.16, 0.10, 0.05, 0.04, 'b'),
            cell(2, 0.22, 0.10, 0.05, 0.04, 'c'),
            cell(3, 0.10, 0.30, 0.05, 0.04, 'd'),
            cell(4, 0.16, 0.30, 0.05, 0.04, 'e'),
            cell(5, 0.22, 0.30, 0.05, 0.04, 'f'),
        ]);
        assert_eq!(pt.lines.len(), 2);
        assert_eq!(pt.line_of(0), Some(0));
        assert_eq!(pt.line_of(5), Some(1));
        assert_eq!(pt.line_start(1), Some(3));
        assert_eq!(pt.line_end(1), Some(5));
    }

    #[test]
    fn range_to_rects_emits_one_rect_per_line() {
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, 'a'),
            cell(1, 0.16, 0.10, 0.05, 0.04, 'b'),
            cell(2, 0.10, 0.30, 0.05, 0.04, 'c'),
            cell(3, 0.16, 0.30, 0.05, 0.04, 'd'),
        ]);
        // Selection from char 0 (line 0) through char 3 (line 1).
        let rects = pt.range_to_rects(0, 3);
        assert_eq!(rects.len(), 2, "got {rects:?}");
    }

    #[test]
    fn char_at_point_exact_hit() {
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, 'a'),
            cell(1, 0.30, 0.10, 0.05, 0.04, 'b'),
        ]);
        assert_eq!(pt.char_at_point(0.12, 0.12), Some(0));
        assert_eq!(pt.char_at_point(0.32, 0.12), Some(1));
    }

    #[test]
    fn char_at_point_snaps_in_whitespace() {
        // Click between two chars; should snap to nearer one.
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, 'a'),
            cell(1, 0.30, 0.10, 0.05, 0.04, 'b'),
        ]);
        // Closer to 'a'.
        assert_eq!(pt.char_at_point(0.18, 0.12), Some(0));
        // Closer to 'b'.
        assert_eq!(pt.char_at_point(0.27, 0.12), Some(1));
    }

    #[test]
    fn extract_inserts_newline_between_lines() {
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, 'a'),
            cell(1, 0.16, 0.10, 0.05, 0.04, 'b'),
            cell(2, 0.10, 0.30, 0.05, 0.04, 'c'),
        ]);
        let s = pt.extract(0, 2);
        assert_eq!(s, "ab\nc");
    }

    #[test]
    fn word_starts_split_on_whitespace() {
        // "ab cd" on one line.
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, 'a'),
            cell(1, 0.16, 0.10, 0.05, 0.04, 'b'),
            cell(2, 0.22, 0.10, 0.05, 0.04, ' '),
            cell(3, 0.28, 0.10, 0.05, 0.04, 'c'),
            cell(4, 0.34, 0.10, 0.05, 0.04, 'd'),
        ]);
        assert_eq!(pt.lines[0].word_starts, vec![0, 3]);
    }

    #[test]
    fn next_word_start_walks_forward() {
        // "ab cd ef" on one line.
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, 'a'),
            cell(1, 0.16, 0.10, 0.05, 0.04, 'b'),
            cell(2, 0.22, 0.10, 0.05, 0.04, ' '),
            cell(3, 0.28, 0.10, 0.05, 0.04, 'c'),
            cell(4, 0.34, 0.10, 0.05, 0.04, 'd'),
            cell(5, 0.40, 0.10, 0.05, 0.04, ' '),
            cell(6, 0.46, 0.10, 0.05, 0.04, 'e'),
            cell(7, 0.52, 0.10, 0.05, 0.04, 'f'),
        ]);
        // From char 0 ('a'), next word start = 3 ('c').
        assert_eq!(pt.next_word_start(0), 3);
        // From char 3 ('c'), next = 6 ('e').
        assert_eq!(pt.next_word_start(3), 6);
        // From last char, returns last.
        assert_eq!(pt.next_word_start(7), 7);
    }

    /// REGRESSION: `w` motion across two adjacent whitespace-only
    /// lines used to land on the *whitespace* of the immediately-
    /// next line because the loop body bailed out after a single
    /// iteration. With multi-line walking the caret now lands on
    /// the actual next word ('c'), matching vim's behaviour.
    #[test]
    fn next_word_start_skips_whitespace_only_lines() {
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, 'a'),
            cell(1, 0.16, 0.10, 0.05, 0.04, 'b'),
            // Line of pure whitespace.
            cell(2, 0.10, 0.30, 0.05, 0.04, ' '),
            cell(3, 0.16, 0.30, 0.05, 0.04, ' '),
            // Real word on line 2.
            cell(4, 0.10, 0.50, 0.05, 0.04, 'c'),
            cell(5, 0.16, 0.50, 0.05, 0.04, 'd'),
        ]);
        // From end of line 0, the next word is 'c' (idx 4) — the
        // whitespace-only line in between must be skipped.
        assert_eq!(pt.next_word_start(1), 4);
    }

    /// REGRESSION: mirror test for `b` (prev_word_start). Walking
    /// backwards from line 2 across a whitespace-only line should
    /// land on the actual previous word, not on the whitespace.
    #[test]
    fn prev_word_start_skips_whitespace_only_lines() {
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, 'a'),
            cell(1, 0.16, 0.10, 0.05, 0.04, 'b'),
            cell(2, 0.10, 0.30, 0.05, 0.04, ' '),
            cell(3, 0.16, 0.30, 0.05, 0.04, ' '),
            cell(4, 0.10, 0.50, 0.05, 0.04, 'c'),
            cell(5, 0.16, 0.50, 0.05, 0.04, 'd'),
        ]);
        // From line 2's first char, the previous word is 'a'
        // (idx 0) — whitespace-only line 1 must be skipped.
        assert_eq!(pt.prev_word_start(4), 0);
    }

    #[test]
    fn end_of_word_lands_on_last_letter() {
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, 'a'),
            cell(1, 0.16, 0.10, 0.05, 0.04, 'b'),
            cell(2, 0.22, 0.10, 0.05, 0.04, ' '),
            cell(3, 0.28, 0.10, 0.05, 0.04, 'c'),
        ]);
        // From 'a', end of word = 1 ('b').
        assert_eq!(pt.end_of_word(0), 1);
        // From the space, end of NEXT word = 3 ('c').
        assert_eq!(pt.end_of_word(2), 3);
    }

    #[test]
    fn word_around_snaps_to_inner_word() {
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, 'a'),
            cell(1, 0.16, 0.10, 0.05, 0.04, 'b'),
            cell(2, 0.22, 0.10, 0.05, 0.04, ' '),
            cell(3, 0.28, 0.10, 0.05, 0.04, 'c'),
            cell(4, 0.34, 0.10, 0.05, 0.04, 'd'),
        ]);
        assert_eq!(pt.word_around(1), Some((0, 1)));
        assert_eq!(pt.word_around(4), Some((3, 4)));
        // From the space, snaps to next word.
        assert_eq!(pt.word_around(2), Some((3, 4)));
    }

    #[test]
    fn find_char_in_line_forward_and_back() {
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, 'a'),
            cell(1, 0.16, 0.10, 0.05, 0.04, 'b'),
            cell(2, 0.22, 0.10, 0.05, 0.04, 'a'),
            cell(3, 0.28, 0.10, 0.05, 0.04, 'c'),
        ]);
        // From 0, next 'a' = 2.
        assert_eq!(pt.find_char_in_line(0, 'a'), Some(2));
        // From 3, prev 'a' = 2.
        assert_eq!(pt.rfind_char_in_line(3, 'a'), Some(2));
        // No match returns None.
        assert_eq!(pt.find_char_in_line(0, 'z'), None);
    }

    /// Regression: pdfium can stream chars from neighbouring lines
    /// interleaved (e.g. footnote sandwiched mid-paragraph). The
    /// `start_idx..=end_idx` span over the line therefore contains
    /// chars from OTHER lines whose stream idx falls inside the
    /// range. `f<c>` must filter by `c.line == line` or it could
    /// match a char on the wrong visual line.
    #[test]
    fn find_char_in_line_skips_chars_from_other_lines() {
        // Stream order: line 0, line 1, line 0, line 1 — so each
        // line's [start_idx, end_idx] span includes the OTHER line's
        // stream indices. char_lines: [0, 1, 0, 1].
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, 'x'), // line 0
            cell(1, 0.10, 0.50, 0.05, 0.04, 'q'), // line 1 (the 'q' we must NOT match)
            cell(2, 0.22, 0.10, 0.05, 0.04, 'y'), // line 0
            cell(3, 0.28, 0.50, 0.05, 0.04, 'r'), // line 1
        ]);
        // From char 0 (line 0), search for 'q' on this line — must
        // return None even though stream idx 1 contains 'q'.
        assert_eq!(pt.find_char_in_line(0, 'q'), None);
        // Conversely from char 1 (line 1), 'x' is on line 0 — None.
        assert_eq!(pt.find_char_in_line(1, 'x'), None);
        // Sanity: same-line lookup still works.
        assert_eq!(pt.find_char_in_line(0, 'y'), Some(2));
        assert_eq!(pt.rfind_char_in_line(3, 'q'), Some(1));
    }

    #[test]
    fn line_first_non_blank_skips_leading_space() {
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, ' '),
            cell(1, 0.16, 0.10, 0.05, 0.04, ' '),
            cell(2, 0.22, 0.10, 0.05, 0.04, 'x'),
        ]);
        assert_eq!(pt.line_first_non_blank(0), Some(2));
    }

    #[test]
    fn extract_skips_generated_chars() {
        let a = cell(0, 0.10, 0.10, 0.05, 0.04, 'a');
        let mut g = cell(1, 0.16, 0.10, 0.05, 0.04, 'X');
        g.is_generated = true;
        let b = cell(2, 0.22, 0.10, 0.05, 0.04, 'b');
        let pt = page_with(vec![a, g, b]);
        let s = pt.extract(0, 2);
        assert_eq!(s, "ab");
    }

    /// REGRESSION: pressing `v` on a page the user had scrolled
    /// halfway through used to anchor the selection at idx 0 (top of
    /// page). Every subsequent caret motion grew the selection above
    /// the viewport, so the live overlay was invisible — the user
    /// only saw a highlight after `y` saved one. Fix: anchor the
    /// caret on the first char whose top edge sits in (or below) the
    /// current viewport. This test pins the helper that drives that.
    #[test]
    fn first_visible_char_idx_skips_chars_above_viewport() {
        use crate::app::first_visible_char_idx_pure;
        // Three rows, top of page at y = 0.05, 0.40, 0.75.
        let pt = page_with(vec![
            cell(0, 0.10, 0.05, 0.05, 0.04, 'a'),
            cell(1, 0.10, 0.40, 0.05, 0.04, 'b'),
            cell(2, 0.10, 0.75, 0.05, 0.04, 'c'),
        ]);
        // Viewport top at 0 → first row (idx 0).
        assert_eq!(first_visible_char_idx_pure(0.0, &pt), Some(0));
        // Viewport top at 0.30 → skips row 0, lands on row 1.
        assert_eq!(first_visible_char_idx_pure(0.30, &pt), Some(1));
        // Viewport top past last row → None.
        assert_eq!(first_visible_char_idx_pure(0.90, &pt), None);
    }

    /// Generated chars (e.g. injected line-end \n) shouldn't anchor
    /// the caret — they have no real x position.
    #[test]
    fn first_visible_char_idx_skips_generated() {
        use crate::app::first_visible_char_idx_pure;
        let mut g = cell(0, 0.10, 0.05, 0.05, 0.04, '\n');
        g.is_generated = true;
        let pt = page_with(vec![
            g,
            cell(1, 0.10, 0.10, 0.05, 0.04, 'a'),
        ]);
        assert_eq!(first_visible_char_idx_pure(0.0, &pt), Some(1));
    }

    /// REGRESSION: the first frame after `v` has anchor==head (single
    /// char). range_to_rects must produce ONE non-empty rect — earlier
    /// versions silently returned zero rects when start==end on the
    /// last char of a single-char line, which would render no live
    /// overlay until the user moved the caret.
    #[test]
    fn range_to_rects_single_char_emits_one_rect() {
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, 'a'),
            cell(1, 0.16, 0.10, 0.05, 0.04, 'b'),
        ]);
        let rects = pt.range_to_rects(0, 0);
        assert_eq!(rects.len(), 1, "anchor==head should produce one rect");
        assert!(rects[0].w > 0.0 && rects[0].h > 0.0);
    }

    /// REGRESSION: the same single-char rect must still appear when
    /// the chosen char is NOT the first on its line.
    #[test]
    fn range_to_rects_single_char_mid_line() {
        let pt = page_with(vec![
            cell(0, 0.10, 0.10, 0.05, 0.04, 'a'),
            cell(1, 0.16, 0.10, 0.05, 0.04, 'b'),
            cell(2, 0.22, 0.10, 0.05, 0.04, 'c'),
        ]);
        let rects = pt.range_to_rects(1, 1);
        assert_eq!(rects.len(), 1);
        // The rect should cover roughly the 'b' cell's x position.
        assert!((rects[0].x - 0.16).abs() < 0.05);
    }
}
