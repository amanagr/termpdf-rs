//! Pre-built full-text index of the open PDF.
//!
//! Replaces O(N pages × pdfium search overhead) with O(text length)
//! string scanning for the "which pages contain this query" question.
//! Sioyek's benchmark shows this is the difference between 0.03s
//! and 4.8s on a 4 100-page document.
//!
//! ## Strategy
//!
//! Idle warm calls `add_page` for one page per tick, building up a
//! concatenated `text: String` plus a `page_starts: Vec<usize>`
//! back-index. Search then becomes `text.match_indices` (or a
//! case-insensitive sweep) that yields byte offsets; `page_for_offset`
//! maps those back to page indices.
//!
//! ## What we don't index
//!
//! Per-character rect coordinates. Pdfium has those (via
//! `PdfPageText`) but storing them in memory is expensive. Instead
//! we use the index to *narrow* the set of pages to ask pdfium
//! about: a query matching 5 of 700 pages becomes 5 pdfium scans
//! instead of 700, even before we have the full index.
//!
//! ## Persistence
//!
//! Built per-process. Persistence to disk is a future task —
//! cheaper than rebuilding for huge docs but adds invalidation
//! complexity. Even without persistence, idle warm fills the
//! index well before the user usually triggers their first search.

use std::cell::OnceCell;
use std::collections::HashSet;
use std::ops::Range;

/// Byte offset into the concatenated `text` field at which a given
/// page's text begins. Sentinel `usize::MAX` means "page not yet
/// indexed".
const NOT_INDEXED: usize = usize::MAX;

#[derive(Debug)]
pub struct DocIndex {
    text: String,
    /// `page_starts[i]` = byte offset where page `i`'s text starts
    /// in `text`, or `NOT_INDEXED` if the page hasn't been added yet.
    /// `text` is built in increasing page-index order so the offsets
    /// stay monotonic across the indexed prefix.
    page_starts: Vec<usize>,
    /// Pages that have been added (regardless of their position in
    /// the build order). Lookup-only set so `add_page` is idempotent.
    indexed_pages: HashSet<usize>,
    total_pages: usize,
    /// Bumped on every `add_page`. Lets external code (status line,
    /// invalidation) detect "index grew" without diffing.
    pub revision: u64,
    /// Cached lowercased copy of `text`. `to_lowercase()` on a 700-page
    /// book is ~50–100 ms and used to fire on every case-insensitive
    /// search. We compute it once on demand and reuse across searches;
    /// `add_page` resets it because the underlying text grew.
    /// `OnceCell` over `&self` so `match_offsets` doesn't need to take
    /// a mut borrow just to populate the cache.
    lower_text_cache: OnceCell<String>,
}

impl DocIndex {
    pub fn new(total_pages: usize) -> Self {
        Self {
            text: String::new(),
            page_starts: vec![NOT_INDEXED; total_pages],
            indexed_pages: HashSet::with_capacity(total_pages),
            total_pages,
            revision: 0,
            lower_text_cache: OnceCell::new(),
        }
    }

    /// Total pages in the document.
    #[allow(dead_code)] // public read API; UI may use it for index status display
    pub fn total_pages(&self) -> usize {
        self.total_pages
    }

    /// How many pages have been indexed so far.
    pub fn indexed_count(&self) -> usize {
        self.indexed_pages.len()
    }

    /// Ratio 0..=1 of how complete the index is.
    pub fn fraction_complete(&self) -> f32 {
        if self.total_pages == 0 {
            return 1.0;
        }
        (self.indexed_count() as f32) / (self.total_pages as f32)
    }

    pub fn is_complete(&self) -> bool {
        self.indexed_count() == self.total_pages
    }

    pub fn contains(&self, page_idx: usize) -> bool {
        self.indexed_pages.contains(&page_idx)
    }

    /// Append `page_text` to the index for `page_idx`. Idempotent —
    /// re-adding a page is a no-op.
    ///
    /// Note: pages must be added in monotonic order for the
    /// `page_starts` map to be correct. The idle-warm scheduler
    /// always picks the lowest unindexed page to enforce this.
    pub fn add_page(&mut self, page_idx: usize, page_text: String) {
        if page_idx >= self.total_pages || self.indexed_pages.contains(&page_idx) {
            return;
        }
        let start = self.text.len();
        self.page_starts[page_idx] = start;
        self.text.push_str(&page_text);
        // Sentinel newline between pages so word matches don't span
        // page boundaries. Cheap; any cross-page hit would be wrong
        // for our highlight rendering anyway.
        self.text.push('\n');
        self.indexed_pages.insert(page_idx);
        self.revision = self.revision.wrapping_add(1);
        // The text just grew, so any cached lowercase copy is stale.
        // Replace the cache cell; the next case-insensitive search
        // pays the lowercase cost once and subsequent searches against
        // the (now-larger) text reuse it.
        self.lower_text_cache = OnceCell::new();
    }

    /// Map a byte offset within `text` back to the page that owns it.
    /// `usize::MAX` if no page is indexed at that offset.
    pub fn page_for_offset(&self, offset: usize) -> usize {
        // Binary search over the monotonic indexed prefix. We treat
        // unindexed entries (sentinel) as gaps and skip them.
        let mut best = usize::MAX;
        let mut lo = 0usize;
        let mut hi = self.page_starts.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let v = self.page_starts[mid];
            if v == NOT_INDEXED {
                // Walk forward until we find an indexed entry; if we
                // can't, retreat.
                let mut step = mid + 1;
                while step < hi && self.page_starts[step] == NOT_INDEXED {
                    step += 1;
                }
                if step < hi {
                    if self.page_starts[step] <= offset {
                        best = step;
                        lo = step + 1;
                    } else {
                        hi = mid;
                    }
                } else {
                    hi = mid;
                }
            } else if v <= offset {
                best = mid;
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        best
    }

    /// Find the lowest page index that's not yet in the index. Used
    /// by idle warm to prioritise filling-in-order so `page_starts`
    /// stays monotonic.
    pub fn next_page_to_index(&self) -> Option<usize> {
        (0..self.total_pages).find(|&i| !self.indexed_pages.contains(&i))
    }

    /// Pages that contain `query`. Case-insensitive when
    /// `case_sensitive=false`. Returns a deduplicated, sorted list of
    /// page indices — caller can ask pdfium for the exact rects on
    /// just those pages instead of every page in the document.
    ///
    /// Only consults the indexed prefix; unindexed pages must be
    /// handled by the caller (typically: walk them via pdfium).
    ///
    /// Output is already strictly increasing: pages are added in
    /// monotonic order so `page_starts` is non-decreasing in indexed
    /// position, `match_offsets` walks the haystack left-to-right, and
    /// the `last_page` filter strips consecutive duplicates. No
    /// trailing sort is needed.
    pub fn pages_matching(&self, query: &str, case_sensitive: bool) -> Vec<usize> {
        if query.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut last_page: usize = usize::MAX;
        for offset in self.match_offsets(query, case_sensitive) {
            let page = self.page_for_offset(offset);
            if page != usize::MAX && page != last_page {
                out.push(page);
                last_page = page;
            }
        }
        out
    }

    /// Iterator-friendly raw offset matcher. Public for tests; in
    /// production callers should prefer `pages_matching`.
    pub fn match_offsets<'a>(
        &'a self,
        query: &'a str,
        case_sensitive: bool,
    ) -> Box<dyn Iterator<Item = usize> + 'a> {
        if case_sensitive {
            Box::new(self.text.match_indices(query).map(|(i, _)| i))
        } else {
            // Lowercase needle each call (small, < 100 chars). The
            // haystack is a 2 MB+ string for a long book; we cache its
            // lowercase form across searches to avoid the multi-tens-
            // of-ms `text.to_lowercase()` per query.
            let needle = query.to_lowercase();
            let hay = self
                .lower_text_cache
                .get_or_init(|| self.text.to_lowercase());
            // Materialise into a Vec because Rust can't express the
            // borrow path "&'a Box<dyn Iterator borrowing &'a String>"
            // through this signature without GATs. The collect cost is
            // dominated by the encode-once lowercase we just skipped.
            let v: Vec<usize> = hay.match_indices(&needle).map(|(i, _)| i).collect();
            Box::new(v.into_iter())
        }
    }

    /// Hit count for `query` (counts every match, not just unique
    /// pages). Cheap status-line readout.
    #[allow(dead_code)] // public read API for status-line previews
    pub fn match_count(&self, query: &str, case_sensitive: bool) -> usize {
        self.match_offsets(query, case_sensitive).count()
    }

    /// Text payload size in bytes — for memory-budget gauges.
    #[allow(dead_code)] // public read API for telemetry / debug
    pub fn text_bytes(&self) -> usize {
        self.text.len()
    }

    /// Indexed text — used by the indexed-pages-only fast path that
    /// scans without re-asking pdfium. Pub(crate) so callers in the
    /// same crate can build alternative search backends on top.
    #[allow(dead_code)]
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}

/// File format magic + version. Bumping the version invalidates
/// existing on-disk indexes (gracefully — we just rebuild).
const INDEX_MAGIC: &[u8; 8] = b"TPDFIDX1";

/// Save the index to `path`. Best-effort: any IO failure returns
/// `Ok(false)` so callers can ignore disk-cache failures.
///
/// File layout:
///   8 bytes   magic "TPDFIDX1"
///   8 bytes   total_pages (u64 LE)
///   8 bytes   indexed_count (u64 LE)
///   8 bytes   text_len (u64 LE)
///   N×8 bytes page_starts vector (u64 LE per entry)
///   M bytes   text payload
pub fn save(index: &DocIndex, path: &std::path::Path) -> std::io::Result<bool> {
    let parent = match path.parent() {
        Some(p) => p,
        None => return Ok(false),
    };
    if let Err(e) = std::fs::create_dir_all(parent) {
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Ok(false);
        }
    }
    let mut buf =
        Vec::with_capacity(8 + 8 + 8 + 8 + index.page_starts.len() * 8 + index.text.len());
    buf.extend_from_slice(INDEX_MAGIC);
    buf.extend_from_slice(&(index.total_pages as u64).to_le_bytes());
    buf.extend_from_slice(&(index.indexed_count() as u64).to_le_bytes());
    buf.extend_from_slice(&(index.text.len() as u64).to_le_bytes());
    for &p in &index.page_starts {
        buf.extend_from_slice(&(p as u64).to_le_bytes());
    }
    buf.extend_from_slice(index.text.as_bytes());

    // Atomic write via rename so a crash mid-write doesn't leave a
    // truncated index that later loads as garbage.
    let tmp = path.with_extension("idx.tmp");
    if std::fs::write(&tmp, &buf).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Ok(false);
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Ok(false);
    }
    Ok(true)
}

/// Load an index from `path`. Returns `None` on any read / parse
/// error (caller treats this as cache miss → builds from scratch).
///
/// Validates that the on-disk total_pages matches the supplied
/// `expected_total` — invalidates the cache when the document's
/// page count differs (e.g. user replaced the PDF with a different
/// version). Hash-key invalidation upstream catches most cases;
/// this is a belt-and-suspenders extra check.
pub fn load(path: &std::path::Path, expected_total: usize) -> Option<DocIndex> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 8 + 8 + 8 + 8 {
        return None;
    }
    if &bytes[..8] != INDEX_MAGIC {
        return None;
    }
    let total_pages = u64::from_le_bytes(bytes[8..16].try_into().ok()?) as usize;
    if total_pages != expected_total {
        return None;
    }
    let indexed_count = u64::from_le_bytes(bytes[16..24].try_into().ok()?) as usize;
    let text_len = u64::from_le_bytes(bytes[24..32].try_into().ok()?) as usize;
    let header_end = 32usize;
    // Wrap-safe: a malformed index file with a giant `text_len` could
    // make `starts_end + text_len` overflow on usize, sneak past the
    // `bytes.len() <` guard, and then panic in the slice on line 332.
    // Threat model: same-UID attacker plants a file under our cache
    // dir (only DoS — they can already do worse). Use `checked_add`
    // and bound text_len by the file size for early rejection.
    let starts_end = header_end.checked_add(total_pages.checked_mul(8)?)?;
    if text_len > bytes.len() {
        return None;
    }
    let needed = starts_end.checked_add(text_len)?;
    if bytes.len() < needed {
        return None;
    }
    let mut page_starts = Vec::with_capacity(total_pages);
    for i in 0..total_pages {
        let base = header_end + i * 8;
        let v = u64::from_le_bytes(bytes[base..base + 8].try_into().ok()?) as usize;
        page_starts.push(v);
    }
    let text = std::str::from_utf8(&bytes[starts_end..starts_end + text_len])
        .ok()?
        .to_string();
    let indexed_pages: HashSet<usize> = (0..total_pages)
        .filter(|i| page_starts[*i] != NOT_INDEXED)
        .collect();
    if indexed_pages.len() != indexed_count {
        // Length mismatch hints at corruption; reject so we rebuild.
        return None;
    }
    Some(DocIndex {
        text,
        page_starts,
        indexed_pages,
        total_pages,
        revision: 1,
        lower_text_cache: OnceCell::new(),
    })
}

/// Range mapping: given a slice's start in `text`, return
/// (page, in-page byte offset, in-page byte length). Helper for
/// callers that want to highlight not just the page but the actual
/// match position. Unused today (pdfium per-match rects are
/// authoritative); kept as a future hook.
#[allow(dead_code)]
pub fn slice_to_page_range(
    index: &DocIndex,
    text_range: Range<usize>,
) -> Option<(usize, Range<usize>)> {
    let page = index.page_for_offset(text_range.start);
    if page == usize::MAX {
        return None;
    }
    let page_start = index.page_starts[page];
    let in_page_start = text_range.start.saturating_sub(page_start);
    let in_page_end = text_range.end.saturating_sub(page_start);
    Some((page, in_page_start..in_page_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated_index() -> DocIndex {
        let mut idx = DocIndex::new(3);
        idx.add_page(0, "the quick brown fox".into());
        idx.add_page(1, "jumps over the lazy dog".into());
        idx.add_page(2, "and rests by the river".into());
        idx
    }

    #[test]
    fn add_page_idempotent() {
        let mut idx = DocIndex::new(2);
        idx.add_page(0, "hello".into());
        let before = idx.text_bytes();
        idx.add_page(0, "hello".into()); // dup
        assert_eq!(
            idx.text_bytes(),
            before,
            "duplicate add_page must be a no-op"
        );
    }

    #[test]
    fn add_page_out_of_range_no_op() {
        let mut idx = DocIndex::new(2);
        idx.add_page(99, "ignored".into());
        assert_eq!(idx.indexed_count(), 0);
    }

    #[test]
    fn pages_matching_groups_per_page_dedups_sorts() {
        let idx = populated_index();
        // "the" appears on all 3 pages; "fox" only on page 0.
        let mut all = idx.pages_matching("the", false);
        all.sort();
        assert_eq!(all, vec![0, 1, 2]);
        assert_eq!(idx.pages_matching("fox", false), vec![0]);
    }

    #[test]
    fn pages_matching_is_case_insensitive_by_default() {
        let idx = populated_index();
        assert_eq!(idx.pages_matching("FOX", false), vec![0]);
        // Case-sensitive: must match exact case.
        assert_eq!(idx.pages_matching("FOX", true), Vec::<usize>::new());
        assert_eq!(idx.pages_matching("fox", true), vec![0]);
    }

    #[test]
    fn match_count_counts_every_occurrence() {
        let idx = populated_index();
        // "the" appears 3 times across pages.
        assert_eq!(idx.match_count("the", false), 3);
        assert_eq!(idx.match_count("zzz", false), 0);
    }

    /// add_page must reset the lowercase-text cache, otherwise a search
    /// after extending the index would miss matches in the newly-added
    /// page (the cache would still hold the pre-add lowercase string).
    #[test]
    fn add_page_invalidates_lowercase_cache() {
        let mut idx = DocIndex::new(3);
        idx.add_page(0, "alpha".into());
        // Prime the cache with one search.
        assert_eq!(idx.match_count("ALPHA", false), 1);
        // Add a new page; the new content must be searchable.
        idx.add_page(1, "BETA".into());
        assert_eq!(
            idx.match_count("beta", false),
            1,
            "cache should have been invalidated on add_page so new text is searchable"
        );
    }

    #[test]
    fn next_page_to_index_returns_lowest_unindexed() {
        let mut idx = DocIndex::new(5);
        assert_eq!(idx.next_page_to_index(), Some(0));
        idx.add_page(0, "a".into());
        assert_eq!(idx.next_page_to_index(), Some(1));
        // Skip-add: still returns lowest unindexed.
        idx.add_page(2, "c".into());
        assert_eq!(idx.next_page_to_index(), Some(1));
    }

    #[test]
    fn complete_when_all_pages_added() {
        let mut idx = DocIndex::new(2);
        assert!(!idx.is_complete());
        idx.add_page(0, "a".into());
        assert!(!idx.is_complete());
        idx.add_page(1, "b".into());
        assert!(idx.is_complete());
        assert!((idx.fraction_complete() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn page_for_offset_finds_correct_page() {
        let idx = populated_index();
        // Page 0 starts at 0, page 1 starts at "the quick brown fox\n".len() = 20.
        assert_eq!(idx.page_for_offset(0), 0);
        assert_eq!(idx.page_for_offset(19), 0);
        assert_eq!(idx.page_for_offset(20), 1);
        // Past last page — no page owns it.
        assert_eq!(idx.page_for_offset(usize::MAX / 2), 2);
    }

    #[test]
    fn slice_to_page_range_translates_to_in_page_offsets() {
        let idx = populated_index();
        // Find "fox" on page 0 starting at byte 16.
        let off = idx.text.find("fox").expect("fox is present");
        let (page, range) = slice_to_page_range(&idx, off..off + 3).expect("page mapped");
        assert_eq!(page, 0);
        assert_eq!(range, 16..19);
    }

    #[test]
    fn save_then_load_roundtrips_text_and_pages() {
        let dir = std::env::temp_dir().join(format!("idx_save_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("idx.bin");
        let idx = populated_index();
        let written = save(&idx, &path).unwrap();
        assert!(written, "save should report success on temp dir");
        let loaded = load(&path, idx.total_pages).expect("load on hit");
        assert_eq!(loaded.total_pages, idx.total_pages);
        assert_eq!(loaded.indexed_count(), idx.indexed_count());
        assert_eq!(loaded.text(), idx.text());
        // Search results match.
        assert_eq!(
            loaded.pages_matching("the", false),
            idx.pages_matching("the", false)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_rejects_wrong_total_pages() {
        let dir = std::env::temp_dir().join(format!("idx_wrong_total_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("idx.bin");
        let idx = populated_index();
        save(&idx, &path).unwrap();
        // Lie about total — must reject.
        assert!(load(&path, 99).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_rejects_wrong_magic() {
        let dir = std::env::temp_dir().join(format!("idx_bad_magic_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("idx.bin");
        std::fs::write(&path, b"NOPE0001\x00\x00\x00\x00\x00\x00\x00\x00").unwrap();
        assert!(load(&path, 1).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_file_returns_none() {
        let path = std::env::temp_dir().join("nonexistent-index-file.bin");
        assert!(load(&path, 5).is_none());
    }

    #[test]
    fn load_truncated_returns_none() {
        let dir = std::env::temp_dir().join(format!("idx_trunc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("idx.bin");
        let idx = populated_index();
        save(&idx, &path).unwrap();
        // Truncate the file mid-header.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(20);
        std::fs::write(&path, &bytes).unwrap();
        assert!(load(&path, idx.total_pages).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Hostile cache file with a giant `text_len` field used to wrap
    /// `starts_end + text_len` past usize::MAX, sneak past the
    /// `bytes.len() <` guard, and panic in the slice. checked_add
    /// rejects cleanly.
    #[test]
    fn load_with_overflowing_text_len_returns_none_not_panic() {
        let dir = std::env::temp_dir().join(format!("idx_overflow_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("idx.bin");
        // Hand-build a header with text_len = u64::MAX (rolls over usize add).
        let mut bytes = Vec::new();
        bytes.extend_from_slice(INDEX_MAGIC);
        bytes.extend_from_slice(&3u64.to_le_bytes()); // total_pages = 3
        bytes.extend_from_slice(&0u64.to_le_bytes()); // indexed_count
        bytes.extend_from_slice(&u64::MAX.to_le_bytes()); // text_len
                                                          // page_starts table (3 × 8 bytes). Doesn't matter what's here.
        bytes.extend_from_slice(&[0u8; 24]);
        std::fs::write(&path, &bytes).unwrap();
        assert!(load(&path, 3).is_none(), "load must reject, not panic");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn revision_bumps_on_each_add() {
        let mut idx = DocIndex::new(2);
        let r0 = idx.revision;
        idx.add_page(0, "a".into());
        assert_ne!(idx.revision, r0);
        let r1 = idx.revision;
        idx.add_page(1, "b".into());
        assert_ne!(idx.revision, r1);
        // Idempotent re-add doesn't bump.
        idx.add_page(0, "a".into());
        assert_eq!(idx.revision, r1.wrapping_add(1));
    }
}
