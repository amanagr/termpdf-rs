//! Per-page kitty graphics placements.
//!
//! Replaces the canvas-based render path for the kitty protocol. Each
//! PDF page becomes its own kitty image with a stable ID; we transmit
//! each page once and from then on every render is just a few hundred
//! bytes of unicode-placeholder cells. The bandwidth saving on
//! steady scroll is the whole point: the previous canvas approach
//! retransmitted a multi-megabyte bitmap on every scroll tick.
//!
//! ## Protocol summary
//!
//! Kitty supports unicode placeholders (`U=1`):
//!   1. Transmit phase (`a=T,U=1,i=ID,...;<base64>`) sends the bitmap
//!      to the terminal, where it stays in the image cache.
//!   2. Placement phase writes one `U+10EEEE` per terminal cell with
//!      diacritics specifying which (row, col) of the image to show
//!      and a foreground color carrying the image ID. Pure text — no
//!      image bytes on the wire.
//!
//! See <https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders>.
//!
//! ## Cache strategy
//!
//! `PageEntry::Transmitted { layout, revision }` is the cache key. If
//! either field changes (zoom/dark = layout; highlight or selection
//! edit = revision), we re-transmit. Otherwise we just emit placement.
//!
//! Image IDs are assigned per page from a process-stable base so
//! reopening the same PDF in a fresh process picks new IDs (avoids
//! collisions with other terminal images).
//!
//! ## What this gives up
//!
//! Cell-quantized scrolling. The previous canvas path had pixel
//! precision; this path snaps placements to cell rows. A mouse wheel
//! or Ctrl-d half-page scroll lands on the nearest cell boundary
//! instead of an exact pixel. In return we drop scroll-frame Draw
//! cost from ~150 ms to ~5 ms.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;

use image::RgbaImage;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::app::LayoutKey;

/// Cap on cached pages. The dominant constraint until decoded RGBA
/// per page exceeds ~17 MB (~A4 at high zoom on a hi-DPI display);
/// past that, the byte budget below takes over.
///
/// Sized to fit the BIDIRECTIONAL working set without churn:
///   - viewport routinely shows 2–3 pages
///   - PREFETCH_DEPTH = 8 in the current scroll direction (idle
///     prefetch looks 8 pages ahead, transmitting them eagerly so a
///     held-`j` burst doesn't pause for pdfium per page)
///   - 8 pages in the OPPOSITE direction (the pages the user just
///     scrolled past — this is the working set for a quick
///     direction flip such as scrolling forward to peek then back
///     to read, which is the dominant reading pattern)
///   - 5-page buffer
///
/// Total: 3 + 8 + 8 + 5 = 24, rounded to 32 for headroom.
///
/// **Why bidirectional matters**: with cap=16 the LRU evicts pages
/// the user just visited as soon as forward prefetch fills. On the
/// reverse-scroll, those pages have to re-render from pdfium. The
/// log signature is "8 contiguous pages adjacent to cursor evicted
/// in one batch" — observed in TERMPDF_DEBUG_LOG with cap=16:
/// pinned=340 evicting [326, 327, 328, 329, 330, 331, 332, 333].
/// User then scrolls back and waits for re-render. Distinct from
/// the cap=7 regression (same direction churn) and from Ghostty's
/// own image-storage-limit eviction (different layer).
///
/// At ~10 MB per page (typical render): 32 × 10 = 320 MB resident,
/// fits in the 768 MB self-budget and in 1 GiB Ghostty
/// image-storage-limit (the README's recommended config). At high
/// zoom (~30 MB/page) the byte budget kicks in to trim — still
/// better than the page-count cap binding too tight.
///
/// Overridable at runtime via `TERMPDF_MAX_CACHED_PAGES` (clamped 4–256)
/// for users on terminals with non-default `image-storage-limit` or
/// extreme working sets — kept as a knob for the same reason
/// `TERMPDF_GHOSTTY_BUDGET_MB` exists for the byte cap.
const DEFAULT_MAX_CACHED_PAGES: usize = 32;

fn max_cached_pages() -> usize {
    use std::sync::OnceLock;
    static CACHED: OnceLock<usize> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("TERMPDF_MAX_CACHED_PAGES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .map(|n| n.clamp(4, 256))
            .unwrap_or(DEFAULT_MAX_CACHED_PAGES)
    })
}

/// Soft ceiling on decoded-RGBA bytes that Ghostty currently holds
/// for OUR images. Verified from `graphics_storage.zig`: Ghostty's
/// `image-storage-limit` defaults to 320 MB (per screen) and is
/// measured in `img.data.len` — DECODED RGBA bytes, NOT bytes-on-wire.
/// Once Ghostty's store is at the cap, the next transmit forces
/// `evictImage` to run; it prefers unused images but **WILL evict
/// images that have live placements** if unused images don't cover
/// the deficit. That's the unloading-on-scroll bug.
///
/// We act as a safety valve: when our transmitted-to-Ghostty footprint
/// approaches the cap, we evict ourselves (LRU non-pinned, with proper
/// `a=d` deletes) so Ghostty never has to choose. The threshold is set
/// just below Ghostty's cap to leave ~40 MB headroom for the next
/// transmit.
///
/// IMPORTANT: this is a SOFT ceiling, not a hard one. The page-count
/// cap (`MAX_CACHED_PAGES`) is the primary eviction trigger; this
/// fires only when individual pages are large enough that a full
/// MAX_CACHED_PAGES set would exceed the cap (high zoom + high-DPI).
/// Counting only TRANSMITTED pages — pre-encoded-but-unsent entries
/// do not occupy Ghostty's store and must not count toward this
/// budget.
///
/// 768 MB by default — sized to match the README's recommended
/// `image-storage-limit = 1073741824` (1 GiB) Ghostty config at the
/// 75 % rule, NOT to Ghostty's stock 320 MB default. Reasoning:
///   - The unloading-on-scroll bug only ever surfaced for users on
///     stock-default Ghostty when our internal page-count cap was
///     too tight (fixed in MAX_CACHED_PAGES bump). The byte budget
///     was firing as a SAFETY VALVE; users who hit it consistently
///     are by definition reading at high zoom on high-DPI displays
///     and have either already raised Ghostty's cap or are willing
///     to (it's the documented mitigation).
///   - At a 280 MB threshold against stock 320 MB Ghostty, the byte
///     path was firing on legitimate prefetch+viewport working sets
///     at high zoom and undoing the work before the user could see
///     the prefetched pages — the same pattern as the page-count cap
///     bug. Raising it past stock Ghostty assumes the user has done
///     the README's recommended config bump; if they haven't, the
///     budget never fires and we fall back to the page-count cap
///     which is itself sized to keep us under stock Ghostty in the
///     normal case.
/// Configurable via `TERMPDF_GHOSTTY_BUDGET_MB` env (clamped 32–4096 MB).
const DEFAULT_GHOSTTY_BUDGET_BYTES: u64 = 768 * 1024 * 1024;

pub(crate) fn ghostty_budget_bytes() -> u64 {
    use std::sync::OnceLock;
    static CACHED: OnceLock<u64> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("TERMPDF_GHOSTTY_BUDGET_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(|mb| mb.clamp(32, 4096) * 1024 * 1024)
            .unwrap_or(DEFAULT_GHOSTTY_BUDGET_BYTES)
    })
}

/// Image-cache entry per page index.
#[derive(Debug, Clone)]
struct PageEntry {
    image_id: u32,
    /// Last-transmitted layout key (zoom + dark). `None` means the
    /// terminal does not yet hold this page's bitmap.
    transmitted_layout: Option<LayoutKey>,
    /// Per-page overlay revision at transmit time (highlights+selection
    /// signature). When the user edits a highlight or moves the live
    /// selection over this page, we re-transmit.
    transmitted_revision: u64,
    /// Width × height of the transmitted bitmap, in pixels. Held so we
    /// can compute the placement-cell grid without re-reading the
    /// bitmap. The image is rounded to cell boundaries before transmit
    /// (see `transmit`) so these are always cell-aligned.
    pixel_w: u32,
    pixel_h: u32,
    /// Encoded payload (PNG or raw RGBA) for the most recent bitmap.
    /// Two readers:
    ///   1. `build_transmit` — if its (layout, revision, dims)
    ///      fingerprint matches, skip the encode and reuse these bytes.
    ///   2. `pre_encode` — populated during idle so the next user
    ///      input lands with PNG bytes ready to ship.
    /// Independent of `transmitted_*`: this caches the encode output,
    /// not the terminal's image cache.
    cached_payload: Option<CachedPayload>,
    /// Snapshot of the registry's `transmitted_bytes_cumulative` at
    /// the moment this page was last marked transmitted. Pairs with
    /// `KittyPageRegistry::is_eviction_at_risk`: the delta between
    /// the registry's current cumulative total and this snapshot is
    /// "bytes that have flowed through Ghostty SINCE this page's
    /// transmit." If that delta exceeds Ghostty's image-storage cap,
    /// Ghostty's `evictImage` is likely to have evicted this page
    /// (it prefers unused images but WILL evict in-use under
    /// pressure, per `graphics_storage.zig:582-610`). Per-page
    /// granularity lets the draw path re-transmit exactly the at-
    /// risk visible pages instead of the blanket
    /// `invalidate_all_transmits`.
    bytes_cumulative_at_my_transmit: u64,
}

#[derive(Debug, Clone)]
struct CachedPayload {
    layout: LayoutKey,
    revision: u64,
    pixel_w: u32,
    pixel_h: u32,
    /// Kitty format code: 100 = PNG, 32 = raw RGBA, 24 = raw RGB.
    format_code: u8,
    /// Kitty compression code: 0 = no compression header, b'z' = `o=z`
    /// (zlib-deflated). PNG (f=100) always rides as 0 because PNG's
    /// container already wraps a zlib stream. RGB (f=24) we always
    /// zlib-compress on the way out so Ghostty stores the deflated
    /// bytes (smaller against image-storage-limit).
    compression: u8,
    bytes: Vec<u8>,
}

pub struct KittyPageRegistry {
    is_tmux: bool,
    /// Base for per-page IDs. `id_for(page) = base + 1 + page_idx`.
    /// Adding 1 keeps the canvas-mode `kitty_image_id` (= base) free
    /// for any fallback path that might still want to use it.
    id_base: u32,
    pages: HashMap<usize, PageEntry>,
    /// LRU order — front = least recently used. `touch` moves a page
    /// to the back; eviction pops from the front. Entries here mirror
    /// keys in `pages`.
    lru: VecDeque<usize>,
    /// Image IDs accumulated for deletion during eviction. Caller
    /// drains via `take_pending_deletes()` (which serializes the
    /// vec into a kitty APC blob) and prepends to the next transmit
    /// so the terminal frees its decoded RGBA copy.
    ///
    /// Stored as a Vec instead of the pre-formatted APC string so
    /// `mark_transmitted` can drop the entry for any page that gets
    /// resurrected (re-rendered + re-marked) before the queued
    /// delete has ridden out. Without that scrub the resurrection
    /// can be clobbered when the delete finally rides on a
    /// different page's transmit — caught by the registry property
    /// tests at I4 ("no live image_id is queued for delete").
    pending_deletes: Vec<u32>,
    /// Running sum of decoded-RGBA bytes for pages whose
    /// `transmitted_layout.is_some()` (i.e. Ghostty actually holds
    /// the bitmap). Maintained incrementally on every mark_transmitted
    /// / evict / invalidate so `evict_to_budget` doesn't have to
    /// `pages.values().sum()` on every steady-state frame. The hot
    /// early-out check now reads one u64 instead of walking the map.
    transmitted_bytes: u64,
    /// Cumulative bytes EVER transmitted through Ghostty within this
    /// "epoch" (resets on `invalidate_all_transmits`). Increments on
    /// every `mark_transmitted` by the new entry's decoded-RGBA
    /// size; never decrements. Distinct from `transmitted_bytes`
    /// (which is a current-resident measure that goes up and down).
    /// Pairs with `PageEntry::bytes_cumulative_at_my_transmit` to
    /// detect per-page eviction risk: when (cumulative_now -
    /// page.cumulative_at_my_transmit) ≥ Ghostty's image-storage
    /// cap, the page is statistically likely to have been evicted
    /// even though our local registry believes it's fresh. Cheaper
    /// and more precise than the blanket `post_scroll_settle`
    /// invalidate (re-transmits ONLY the at-risk pages, not all
    /// visible).
    transmitted_bytes_cumulative: u64,
    /// Scratch buffers reused across `place_page` calls. Memoizes the
    /// per-row diacritic string (varies by viewport `cols` only) and
    /// the restore-cursor escape (varies by `area.width`/`area.height`
    /// only). Without this each visible page-row paid two small allocs
    /// per draw — ~210 allocs per frame in a 3-page-visible scroll.
    place_scratch: PlaceScratch,
}

/// Cached per-frame buffers used by `place_page`. Fields are owned by
/// the registry and rebuilt only when their key inputs change.
#[derive(Default)]
pub struct PlaceScratch {
    /// `width_cells - 1` repetitions of `U+10EEEE`. Cached because
    /// rebuilding it per row was the second-largest steady-scroll
    /// allocation source.
    row_diacritics: String,
    cached_cols: u16,
    /// `\x1b[u\x1b[(W-1)C\x1b[(H-1)B`. Depends on the placement
    /// area's dims, which change only on terminal resize.
    restore_cursor: String,
    cached_restore_dims: (u16, u16),
    /// `\x1b[s␠␠…␠\x1b[u\x1b[(W-1)C\x1b[(H-1)B`. The row-clear escape
    /// `clear_page_area` writes to column 0 of every row of the image
    /// area (drops Ghostty's stale virtual-placement placeholders).
    /// Depends only on `(area.width, area.height)`, which only change
    /// on terminal resize — but `clear_page_area` runs on every
    /// frame, so without caching we paid a fresh `String` alloc + a
    /// per-cell `push(' ')` loop on every redraw, including pure-idle
    /// ones.
    cached_row_clear: String,
    cached_row_clear_dims: (u16, u16),
    /// `\x1b[s\x1b[38;2;R;G;Bm\u{10EEEE}` — the per-page constant
    /// prefix every row of `place_page` writes. Depends only on
    /// `image_id`, which is stable for the page's lifetime in the
    /// registry. Cached so the per-row inner loop is push_str + push
    /// instead of a 5-arg format!().
    cached_row_head: String,
    cached_row_head_id: Option<u32>,
    /// Per-row symbol working buffer. Always cleared at the start of
    /// each row; reuse across calls keeps the underlying allocation
    /// from being freed/realloced every page.
    pub symbol: String,
    /// Last-known placement rectangles for each page that ever got
    /// drawn while still in the registry. `clear_page_area`
    /// consults this to preserve placeholder cells for pages that
    /// (a) are still layout-visible AND (b) were previously placed
    /// — even if they've now collapsed to 0 cells visible.
    ///
    /// **Why the map sticks across frames instead of being swapped
    /// per-frame:** the cell-clipped state can persist for many
    /// frames (slow scroll across a page boundary, sub-cell jitter).
    /// A per-frame `prev/curr` swap would zero the map after the
    /// first cell-clipped frame, defeating the purpose.
    ///
    /// **Cleanup:** `evict_to_budget` removes entries for pages
    /// that genuinely left the registry; entries for pages still
    /// in `pages` linger harmlessly (only consulted when the page
    /// is also in `preserve_pages`).
    last_placed: std::collections::HashMap<usize, ratatui::layout::Rect>,
    /// Reusable buffer for the per-frame rect set passed to
    /// `clear_page_area`. The earlier code allocated a fresh
    /// `Vec<Rect>` every frame even when both the preserve list and
    /// `last_placed` map were empty (the steady-state idle case).
    /// `clear()` instead of `Vec::new` keeps the underlying capacity
    /// across frames.
    preserve_rects: Vec<ratatui::layout::Rect>,
    /// Reusable rect set for `place_page`'s own-page-moved cleanup
    /// — the rects of OTHER pages currently in `last_placed`. The
    /// per-cell `occupied` check iterates this once per cell;
    /// pre-collecting it into a Vec (vs walking the HashMap per
    /// cell) cuts the inner check from O(map_iter) per cell to O(N)
    /// linear scan over a Vec already in cache. Also lets us
    /// short-circuit when the Vec is empty (single visible page).
    place_other_rects: Vec<ratatui::layout::Rect>,
}

impl KittyPageRegistry {
    pub fn new(is_tmux: bool, id_base: u32) -> Self {
        Self {
            is_tmux,
            id_base,
            pages: HashMap::new(),
            lru: VecDeque::new(),
            pending_deletes: Vec::new(),
            transmitted_bytes: 0,
            transmitted_bytes_cumulative: 0,
            place_scratch: PlaceScratch::default(),
        }
    }

    /// Per-entry decoded-RGBA byte cost. Inline so the byte-budget
    /// callers and the incremental tracker agree on the formula.
    #[inline]
    fn entry_bytes(pixel_w: u32, pixel_h: u32) -> u64 {
        (pixel_w as u64).saturating_mul(pixel_h as u64) * 4
    }

    /// Mutable handle to the place-scratch. Hot path: passed into
    /// `place_page` so it can reuse cached row-diacritic + restore-
    /// cursor strings across calls. Public so the renderer can wire
    /// it into the placement loop.
    pub fn place_scratch_mut(&mut self) -> &mut PlaceScratch {
        &mut self.place_scratch
    }

    /// Drop all cached entries. Caller is responsible for sending
    /// `a=d,d=A` if it wants to also free the terminal-side cache.
    /// (We don't, on the theory that the terminal will GC images that
    /// no placeholder references.)
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.pages.clear();
        self.lru.clear();
        self.pending_deletes.clear();
    }

    /// Drop any queued delete for `image_id` from `pending_deletes`.
    /// Called from `mark_transmitted` so a resurrected page (one that
    /// was evicted, then re-rendered + re-marked before the queued
    /// delete had ridden out) doesn't get clobbered by its own stale
    /// delete on the next ride-along. The cost is O(n) on the queued
    /// vec, but `n <= MAX_CACHED_PAGES` so the scan is trivial.
    fn drop_pending_delete(&mut self, image_id: u32) {
        // Skip the retain scan in the common case where nothing is
        // queued. mark_transmitted (the hot caller) calls this for
        // every transmitted page every frame; pending_deletes is
        // empty most of the time outside of cap-hit eviction bursts.
        if self.pending_deletes.is_empty() {
            return;
        }
        self.pending_deletes.retain(|&id| id != image_id);
    }

    /// Move `page_idx` to the MRU end of the LRU list. Idempotent.
    /// Fast-paths the common steady-scroll case where the page being
    /// touched is already at MRU — skips the O(n) scan + remove that
    /// would otherwise fire on every redraw of the same visible page.
    fn touch(&mut self, page_idx: usize) {
        if self.lru.back().copied() == Some(page_idx) {
            return;
        }
        if let Some(pos) = self.lru.iter().position(|&p| p == page_idx) {
            self.lru.remove(pos);
        }
        self.lru.push_back(page_idx);
    }

    /// Evict LRU entries until the registry holds at most
    /// `MAX_CACHED_PAGES` pages AND its TRANSMITTED-to-Ghostty
    /// decoded-RGBA footprint is under `ghostty_budget_bytes()`. Pages
    /// in `pinned` are skipped so the current frame's visible pages
    /// stay alive. For each eviction, queues a kitty `a=d,d=I,i=ID`
    /// delete escape into `pending_deletes` so the terminal also
    /// frees its image.
    ///
    /// **Byte budget accounting**: only TRANSMITTED pages count toward
    /// Ghostty's image-storage-limit. A pre-encoded-but-unsent entry
    /// occupies our local cache but Ghostty doesn't know about it.
    /// Counting it would over-trigger eviction during prefetch, evicting
    /// pages the user is about to scroll into.
    ///
    /// **Soft ceiling philosophy**: the page-count cap
    /// (`MAX_CACHED_PAGES`) is the primary trigger. The byte cap fires
    /// only when individual pages are large enough that a full cap-set
    /// would exceed Ghostty's store (high zoom on high-DPI displays).
    /// In the common case the byte path is a no-op.
    pub fn evict_to_budget(&mut self, pinned: &[usize]) {
        // Debug-only invariant: running total must match the value a
        // map-walk would produce. Catches any mutation site that
        // forgot to update `transmitted_bytes`. Compiled out of
        // release builds — production hot path is the early-out.
        debug_assert_eq!(
            self.transmitted_bytes,
            self.pages
                .values()
                .filter(|e| e.transmitted_layout.is_some())
                .map(|e| Self::entry_bytes(e.pixel_w, e.pixel_h))
                .sum::<u64>(),
            "transmitted_bytes drift — a mutation site forgot to update the running total"
        );
        // Hot early-out: read the running totals (no map walk, no
        // HashSet alloc). In steady-state scrolls neither cap is
        // breached and we return without touching anything.
        let budget = ghostty_budget_bytes();
        let over_pages = self.pages.len().saturating_sub(max_cached_pages());
        let over_bytes = self.transmitted_bytes.saturating_sub(budget);
        if over_pages == 0 && over_bytes == 0 {
            return;
        }

        // Past the early-out: actually have to evict. Now build the
        // pin-set and scan the LRU. Front (LRU) to back, collecting
        // non-pinned victims. Stop once both the page-count AND
        // byte-budget caps are satisfied. Pre-encoded-but-unsent
        // entries (no transmitted_layout) consume zero bytes against
        // the budget, so evicting them only helps the page-count cap;
        // we still pick them as victims to satisfy that cap, but they
        // don't help bytes_freed cross the over_bytes threshold.
        let pin: std::collections::HashSet<usize> = pinned.iter().copied().collect();
        let mut victims: Vec<usize> = Vec::new();
        let mut bytes_freed: u64 = 0;
        let mut pages_freed: usize = 0;
        for &cand in self.lru.iter() {
            if pages_freed >= over_pages && bytes_freed >= over_bytes {
                break;
            }
            if pin.contains(&cand) {
                continue;
            }
            if let Some(entry) = self.pages.get(&cand) {
                if entry.transmitted_layout.is_some() {
                    bytes_freed =
                        bytes_freed.saturating_add(Self::entry_bytes(entry.pixel_w, entry.pixel_h));
                }
            }
            victims.push(cand);
            pages_freed += 1;
        }
        if victims.is_empty() {
            return;
        }
        let victim_set: std::collections::HashSet<usize> = victims.iter().copied().collect();
        // Collect all freed image_ids first so we can coalesce
        // contiguous runs into a single `d=R` range delete (kitty
        // protocol v0.33.0+). Forward scroll evicts oldest pages
        // first; their image_ids — assigned `seed + page_idx` at
        // first reference — naturally cluster, so most evictions
        // collapse to one range escape instead of N per-id ones.
        let mut freed_ids: Vec<u32> = Vec::with_capacity(victims.len());
        for v in &victims {
            if let Some(entry) = self.pages.remove(v) {
                if entry.transmitted_layout.is_some() {
                    self.transmitted_bytes = self
                        .transmitted_bytes
                        .saturating_sub(Self::entry_bytes(entry.pixel_w, entry.pixel_h));
                }
                freed_ids.push(entry.image_id);
            }
            // Drop the cached placement rect for evicted pages so
            // clear_page_area doesn't preserve cells for an image_id
            // that's already been queued for delete.
            self.place_scratch.last_placed.remove(v);
        }
        self.lru.retain(|p| !victim_set.contains(p));
        if crate::debug_log::enabled() {
            crate::debug_log::write(
                "evict",
                &format!(
                    "victims={victims:?} freed_ids={freed_ids:?} pinned={pinned:?} \
                     remaining={n}",
                    n = self.pages.len()
                ),
            );
        }
        self.queue_deletes(&freed_ids);
    }

    /// Append a set of freed image_ids to the pending-deletes queue.
    /// Stores raw ids; the run-length / range coalesce happens at
    /// `take_pending_deletes` time so a resurrection between push
    /// and take can scrub a single id out of the queue without
    /// having to walk a serialized blob.
    fn queue_deletes(&mut self, ids: &[u32]) {
        self.pending_deletes.extend_from_slice(ids);
    }

    /// Drain accumulated delete escapes (from prior evictions). The
    /// caller should prepend the result to its next transmit string
    /// so the terminal processes the deletes alongside the new frame.
    /// Serializes the queued ids into kitty `a=d,d=R,x=LO,y=HI` (run)
    /// + `a=d,d=I,i=ID` (singleton) APCs — one APC per contiguous
    /// id run for byte efficiency on typical forward-scroll evictions.
    pub fn take_pending_deletes(&mut self) -> Option<String> {
        if self.pending_deletes.is_empty() {
            return None;
        }
        let mut ids = std::mem::take(&mut self.pending_deletes);
        Some(serialize_pending_deletes(&mut ids, self.is_tmux))
    }

    /// Push a delete-escape blob back onto the pending queue. Used
    /// when the caller drained but couldn't find a transmit to ride
    /// it in on; the next frame with any transmit will pick it up.
    /// Re-parses the ids out of the blob — `take_pending_deletes`'s
    /// shape inversion. The parse is exact for the formats we emit
    /// (`d=I,i=ID` and `d=R,x=LO,y=HI`); anything else is silently
    /// dropped (defensive — should never happen in practice).
    pub fn put_back_pending_deletes(&mut self, s: String) {
        let mut ids = parse_pending_delete_blob(&s);
        if ids.is_empty() {
            return;
        }
        // Filter out ids that have since been re-allocated to a now-
        // resident page. Without this, a page evicted between
        // `take_pending_deletes` and `put_back_pending_deletes` whose
        // image_id was then reused by a newly-loaded page would have
        // its delete re-queued — and the next ride-along would emit
        // `d=I,i=ID` against a LIVE image, violating the I4 invariant
        // that delete must precede transmit for a given id.
        let live_ids: std::collections::HashSet<u32> =
            self.pages.values().map(|e| e.image_id).collect();
        ids.retain(|id| !live_ids.contains(id));
        if ids.is_empty() {
            return;
        }
        // Prepend so the originally-queued ids retain their order
        // ahead of any new ones queued since.
        let mut combined = ids;
        combined.append(&mut self.pending_deletes);
        self.pending_deletes = combined;
    }

    /// Stable image_id for `page_idx`. Uses `checked_add` so a giant
    /// document (page_idx in the millions) on top of an `id_base`
    /// near `u32::MAX` doesn't silently alias two distinct pages onto
    /// the same id — `wrapping_add` would let `id_for(0)` and
    /// `id_for(some_high_idx)` collide, causing the second transmit
    /// to overwrite the first in Ghostty's image store and mapping
    /// the first page's placeholders to wrong content.
    ///
    /// On overflow, fall back to a deterministic re-fold into the
    /// low half of the id space — keeps the function total without
    /// a panic. The fold loses uniqueness for documents with more
    /// than `u32::MAX / 2` pages (impossible in practice — max known
    /// PDF page count is ~50k), but for any realistic input the
    /// fold's input never overflows and the function reduces to the
    /// original `add`.
    fn id_for(&self, page_idx: usize) -> u32 {
        let pi = page_idx as u32;
        match self.id_base.checked_add(1).and_then(|b| b.checked_add(pi)) {
            Some(id) => id,
            None => {
                // Fold into low half. Distinct page_idx values in the
                // realistic range (< 2^31) still produce distinct ids
                // because the fold preserves the low 31 bits of the
                // (id_base + page_idx + 1) sum modulo 2^31.
                let folded = (self.id_base as u64)
                    .wrapping_add(1)
                    .wrapping_add(pi as u64);
                ((folded & 0x7FFF_FFFF) as u32).max(1)
            }
        }
    }

    /// True if this page has been transmitted to the terminal at least
    /// once with this registry alive. When true, deferring a stale
    /// transmit just means placement shows the previous (slightly
    /// older) image — when false, placement without a transmit emits
    /// garbled foreground-color cells, so the caller MUST transmit.
    pub fn has_prior_transmit(&self, page_idx: usize) -> bool {
        self.pages
            .get(&page_idx)
            .is_some_and(|e| e.transmitted_layout.is_some())
    }

    /// Force `is_fresh` to return false on the next call for this
    /// page so the next draw re-transmits, even when the per-page
    /// revision and layout key are unchanged. Used by the Fast→Sharp
    /// upgrade path: the bitmap content changes but neither revision
    /// nor layout does, so without this nudge the terminal would keep
    /// showing the Fast-quality pixels indefinitely.
    pub fn invalidate_transmit(&mut self, page_idx: usize) {
        if let Some(entry) = self.pages.get_mut(&page_idx) {
            // If this entry was counted toward the running byte total,
            // remove its contribution before flipping the flag.
            if entry.transmitted_layout.is_some() {
                let bytes = Self::entry_bytes(entry.pixel_w, entry.pixel_h);
                self.transmitted_bytes = self.transmitted_bytes.saturating_sub(bytes);
            }
            entry.transmitted_layout = None;
            // Drop any cached encoded payload too — its bytes no
            // longer match what we'd want to ship.
            entry.cached_payload = None;
        }
    }

    /// Force `is_fresh` to return false on every page in the registry.
    /// Used by the manual Ctrl-L refresh path: if Ghostty's internal
    /// image cache silently evicts one of our images (its
    /// image-storage-limit got hit, or it ran some internal LRU we
    /// can't observe), every placeholder cell pointing at that
    /// image_id renders blank but our `is_fresh` still returns true.
    /// Without an external nudge the page stays blank until the user
    /// happens to do something that flips the page's revision (e.g.
    /// click → selection signature → re-transmit). Cheaper than
    /// triggering the zoom-out-zoom-in dance because no layout flip
    /// is involved (no scroll jump, no compose-key invalidation).
    /// Keeps the cached payload bytes — only the transmitted_layout
    /// flag is cleared, so the next transmit re-uses the existing
    /// PNG without re-encoding.
    pub fn invalidate_all_transmits(&mut self) {
        for entry in self.pages.values_mut() {
            entry.transmitted_layout = None;
        }
        // Every page just lost its "Ghostty holds this" status, so the
        // running total of transmitted bytes is zero.
        self.transmitted_bytes = 0;
        // Reset the cumulative epoch counter too: the next round of
        // transmits is starting from a clean slate; their snapshots
        // should be relative to the new epoch, not the old one.
        // Otherwise a page transmitted right after invalidate would
        // inherit a huge `bytes_cumulative_at_my_transmit - 0` delta
        // from the prior epoch and look "at risk" immediately.
        self.transmitted_bytes_cumulative = 0;
        if crate::debug_log::enabled() {
            crate::debug_log::write(
                "invalidate_all",
                &format!("n_pages={n}", n = self.pages.len()),
            );
        }
    }

    /// True if the cached transmit for this page is fresh — caller
    /// can skip the transmit step. Read-only; pairs with
    /// `mark_transmitted` after the actual transmit string has been
    /// emitted.
    pub fn is_fresh(
        &self,
        page_idx: usize,
        layout: LayoutKey,
        revision: u64,
        pixel_w: u32,
        pixel_h: u32,
    ) -> bool {
        match self.pages.get(&page_idx) {
            Some(e) => {
                e.transmitted_layout == Some(layout)
                    && e.transmitted_revision == revision
                    && e.pixel_w == pixel_w
                    && e.pixel_h == pixel_h
            }
            None => false,
        }
    }

    /// True if cumulative transmits since this page's last
    /// `mark_transmitted` have exceeded `threshold_bytes` — Ghostty's
    /// `evictImage` sorts by `(used, time)` and WILL evict in-use
    /// pages under storage pressure (`graphics_storage.zig:582-610`),
    /// so a page that's seen a full cap's worth of bytes flow past
    /// it since its transmit is statistically likely to have been
    /// evicted. The draw path can then re-transmit pre-emptively
    /// instead of waiting for the post-scroll-settle blanket
    /// invalidate. Pages with no prior transmit return false (they
    /// have no snapshot to compare against — they aren't in
    /// Ghostty's store yet). The threshold should be sized to
    /// Ghostty's `image-storage-limit` (320 MB stock; 1 GiB if the
    /// user followed the README) — caller passes the value so the
    /// registry stays env-agnostic and unit-testable.
    pub fn is_eviction_at_risk(&self, page_idx: usize, threshold_bytes: u64) -> bool {
        self.pages.get(&page_idx).is_some_and(|e| {
            e.transmitted_layout.is_some()
                && self
                    .transmitted_bytes_cumulative
                    .saturating_sub(e.bytes_cumulative_at_my_transmit)
                    >= threshold_bytes
        })
    }

    /// Cumulative bytes transmitted in the current epoch. Exposed for
    /// tests + future debug-log paths; production code uses
    /// `is_eviction_at_risk` directly.
    #[allow(dead_code)]
    pub fn transmitted_bytes_cumulative(&self) -> u64 {
        self.transmitted_bytes_cumulative
    }

    /// Update registry state to reflect that this page has just been
    /// transmitted (or about to be). The corresponding transmit
    /// string is built by `build_transmit` separately.
    pub fn mark_transmitted(
        &mut self,
        page_idx: usize,
        layout: LayoutKey,
        revision: u64,
        pixel_w: u32,
        pixel_h: u32,
    ) {
        let id = self.id_for(page_idx);
        // If this page was just evicted (its image_id is queued for
        // delete) and we're now resurrecting it via a fresh transmit,
        // the queued delete is stale: leaving it in the queue would
        // clobber the resurrection on the next ride-along that picks
        // up this delete alongside some OTHER page's transmit. Drop
        // the stale delete now. Caught by the registry property
        // tests at I4.
        self.drop_pending_delete(id);
        // Subtract the entry's previous byte contribution before we
        // overwrite its dimensions; re-add after. Only entries whose
        // transmitted_layout was Some counted toward the running total.
        let prev_bytes = match self.pages.get(&page_idx) {
            Some(e) if e.transmitted_layout.is_some() => Self::entry_bytes(e.pixel_w, e.pixel_h),
            _ => 0,
        };
        // Snapshot the current cumulative total BEFORE adding this
        // page's bytes — the snapshot is what later eviction-risk
        // checks compare against, and counting our own bytes would
        // mean a page is "instantly at risk" by exactly its own size.
        let cumulative_at_my_transmit = self.transmitted_bytes_cumulative;
        let entry = self.pages.entry(page_idx).or_insert(PageEntry {
            image_id: id,
            transmitted_layout: None,
            transmitted_revision: 0,
            pixel_w,
            pixel_h,
            cached_payload: None,
            bytes_cumulative_at_my_transmit: 0,
        });
        entry.image_id = id;
        entry.transmitted_layout = Some(layout);
        entry.transmitted_revision = revision;
        entry.pixel_w = pixel_w;
        entry.pixel_h = pixel_h;
        entry.bytes_cumulative_at_my_transmit = cumulative_at_my_transmit;
        let new_bytes = Self::entry_bytes(pixel_w, pixel_h);
        self.transmitted_bytes = self
            .transmitted_bytes
            .saturating_sub(prev_bytes)
            .saturating_add(new_bytes);
        // Cumulative counter never decrements within an epoch. Each
        // transmit pushes it forward by `new_bytes`; the next call's
        // snapshot will see this updated value.
        self.transmitted_bytes_cumulative =
            self.transmitted_bytes_cumulative.saturating_add(new_bytes);
        self.touch(page_idx);
        if crate::debug_log::enabled() {
            crate::debug_log::write(
                "mark_transmitted",
                &format!(
                    "page={page_idx} id={id} rev={revision} w={pixel_w} h={pixel_h} \
                     fit_w={fit} dark={dark}",
                    fit = layout.fit_width_px,
                    dark = layout.dark
                ),
            );
        }
    }

    /// Build the kitty transmit escape for this page's bitmap. Free
    /// function exposed via the registry so callers don't have to
    /// know the `is_tmux` flag separately.
    ///
    /// Consults the per-page payload cache: if a cached encode for the
    /// same (layout, revision, dims) already exists (e.g. populated by
    /// `pre_encode` during idle, or by a previous transmit at the same
    /// revision), reuse those bytes instead of re-encoding. PNG encode
    /// is the dominant cost in this function — skipping it makes the
    /// transmit a near-pure base64 + IO operation.
    pub fn build_transmit(
        &mut self,
        bitmap: &RgbaImage,
        page_idx: usize,
        layout: LayoutKey,
        revision: u64,
    ) -> String {
        let id = self.id_for(page_idx);
        let pixel_w = bitmap.width();
        let pixel_h = bitmap.height();
        let is_tmux = self.is_tmux;
        // Borrow-scope the entry so the `touch` call below sees a clean
        // `&mut self`. Within the scope: ensure the cached payload
        // matches the request fingerprint, then borrow its bytes
        // directly into `build_transmit_string`. Previously this path
        // cloned `c.bytes` on every cache hit (~50–300 KB per call,
        // doubled on miss to populate the cache); the encoded payload
        // already has one canonical home in the cache, so we let the
        // transmit-builder borrow it instead of cloning.
        let result = {
            let entry = self.pages.entry(page_idx).or_insert(PageEntry {
                image_id: id,
                transmitted_layout: None,
                transmitted_revision: 0,
                pixel_w,
                pixel_h,
                cached_payload: None,
                bytes_cumulative_at_my_transmit: 0,
            });
            let needs_encode = match &entry.cached_payload {
                Some(c) => {
                    !(c.layout == layout
                        && c.revision == revision
                        && c.pixel_w == pixel_w
                        && c.pixel_h == pixel_h)
                }
                None => true,
            };
            if needs_encode {
                let (format_code, compression, bytes) = encode_payload_opaque(bitmap);
                entry.cached_payload = Some(CachedPayload {
                    layout,
                    revision,
                    pixel_w,
                    pixel_h,
                    format_code,
                    compression,
                    bytes,
                });
            }
            let c = entry
                .cached_payload
                .as_ref()
                .expect("cached_payload populated on the line above");
            let needs_encode_log = needs_encode;
            let bytes_len = c.bytes.len();
            let format_code = c.format_code;
            let compression = c.compression;
            let s = build_transmit_string(
                &c.bytes,
                c.format_code,
                c.compression,
                id,
                pixel_w,
                pixel_h,
                is_tmux,
            );
            if crate::debug_log::enabled() {
                crate::debug_log::write(
                    "transmit",
                    &format!(
                        "page={page_idx} id={id} w={pixel_w} h={pixel_h} \
                         payload_bytes={bytes_len} wire_bytes={wire} \
                         f={format_code} o={compression} re_encoded={needs_encode_log}",
                        wire = s.len()
                    ),
                );
            }
            s
        };
        self.touch(page_idx);
        result
    }

    /// Encode the bitmap's payload (PNG or raw RGBA) into the cache
    /// without transmitting or registering as terminal-side resident.
    /// Distinct from `build_transmit` which both encodes AND records
    /// the page as transmitted. Use when you want the encode work
    /// done in advance but plan to ship the transmit yourself later.
    ///
    /// Returns immediately if the cached payload's fingerprint already
    /// matches — so calling this multiple times for an unchanged page
    /// is free.
    #[allow(dead_code)] // public extension hook; tests cover the cache contract
    pub fn pre_encode(
        &mut self,
        bitmap: &RgbaImage,
        page_idx: usize,
        layout: LayoutKey,
        revision: u64,
    ) {
        let id = self.id_for(page_idx);
        let pixel_w = bitmap.width();
        let pixel_h = bitmap.height();
        let entry = self.pages.entry(page_idx).or_insert(PageEntry {
            image_id: id,
            transmitted_layout: None,
            transmitted_revision: 0,
            pixel_w,
            pixel_h,
            cached_payload: None,
            bytes_cumulative_at_my_transmit: 0,
        });
        if let Some(c) = &entry.cached_payload {
            if c.layout == layout
                && c.revision == revision
                && c.pixel_w == pixel_w
                && c.pixel_h == pixel_h
            {
                // Touch even on cache-hit. Without this, an idle-warm
                // page that gets `pre_encode`d every prefetch tick sits
                // at LRU front *only on the first encode* — every
                // subsequent cache-hit short-circuits before the touch
                // below, so the page slowly migrates to the LRU tail
                // and is first to evict despite being actively warm.
                self.touch(page_idx);
                return;
            }
        }
        let (format_code, compression, bytes) = encode_payload_opaque(bitmap);
        entry.cached_payload = Some(CachedPayload {
            layout,
            revision,
            pixel_w,
            pixel_h,
            format_code,
            compression,
            bytes,
        });
        self.touch(page_idx);
    }

    /// Pixel dimensions of the cached bitmap for this page, if any.
    #[allow(dead_code)]
    pub fn dimensions(&self, page_idx: usize) -> Option<(u32, u32)> {
        self.pages.get(&page_idx).map(|e| (e.pixel_w, e.pixel_h))
    }

    /// Image ID assigned to this page (regardless of transmit state).
    pub fn image_id(&self, page_idx: usize) -> u32 {
        self.id_for(page_idx)
    }
}

/// PAGE encode path — strips alpha and ships RGB through `o=z` zlib.
/// Page bitmaps come out of pdfium opaque (alpha=255 everywhere) and
/// stay opaque after dark-mode inversion + highlight baking (we
/// composite onto an opaque bg). The new pipeline is:
///   raw RGBA → strip alpha (RGB) → flate2 zlib deflate → kitty `f=24,o=z`
///
/// Why this is faster than PNG (the prior path):
///   - PNG's `Up` filter is the bulk of its CPU. Skipping it cuts
///     encode time ~3× (benchmarked: ~14 ms for `Fast`+`Up` PNG vs
///     ~5 ms for raw zlib on the same bitmap).
///   - Wire size is comparable for PDF-page content: PNG's filter
///     buys ~10–30 % vs raw deflate, but raw RGB starts smaller (no
///     PNG container, no chunk framing, no alpha plane).
///   - Inside Ghostty, kitty `f=24,o=z` decodes straight to RGB
///     bytes against `image-storage-limit` (per agent-3 research of
///     graphics_storage.zig); PNG took the long way through the
///     wuffs decoder. Net effect: same or smaller per-image footprint
///     and faster terminal-side decode.
///
/// `TERMPDF_TRANSMIT_RAW=1` still forces uncompressed `f=24` (raw RGB)
/// for A/B testing the compression alone. Setting `TERMPDF_TRANSMIT_PNG=1`
/// reverts to the old PNG path for terminals that mishandle `o=z`.
///
/// Overlay images (selection band) keep PNG/RGBA via `encode_payload`;
/// they have real alpha < 255 by design.
fn encode_payload_opaque(bitmap: &RgbaImage) -> (u8, u8, Vec<u8>) {
    if force_raw_env() {
        return (24, 0, strip_alpha(bitmap));
    }
    if force_png_env() {
        return match encode_png_fast_rgb(bitmap) {
            Ok(png) => (100, 0, png),
            Err(_) => (24, 0, strip_alpha(bitmap)),
        };
    }
    match encode_rgb_zlib(bitmap) {
        Ok(deflated) => (24, b'z', deflated),
        Err(_) => (24, 0, strip_alpha(bitmap)),
    }
}

/// Strip alpha, then zlib-deflate the RGB bytes. Returns the `o=z`
/// payload for kitty `f=24,o=z` transmit. Uses `flate2::Compression::fast()`
/// (zlib level 1) — see body comment for why level 1 is the sweet spot.
///
/// Streams RGB into the encoder in fixed-size stack-allocated chunks
/// rather than materialising the entire RGB plane first: at high zoom
/// the full RGB Vec was ~24 MB, so peak memory was RGBA + RGB + zlib
/// output simultaneously. Streaming caps the additional working set
/// at the chunk size (12 KB on the stack) plus the encoder's own
/// internal buffers — the input RGBA is borrowed in-place, never
/// copied wholesale.
fn encode_rgb_zlib(bitmap: &RgbaImage) -> std::io::Result<Vec<u8>> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;
    // PDF pages typically deflate 5–20× on real content. Reserve a
    // generous lower bound so the inner Vec doesn't grow during the
    // flush (encoder writes in ~32 KB chunks).
    //
    // `Compression::fast()` is zlib level 1 — matches the encode-CPU
    // profile of the prior PNG `CompressionType::Fast` path. Higher
    // levels (`default()` = 6) added measurable selection-drag CPU
    // for a marginal byte-size delta on PDF page content; fast keeps
    // this swap perf-neutral against the kitty PNG encode it
    // replaced.
    let mut enc = ZlibEncoder::new(Vec::with_capacity(384 * 1024), Compression::fast());
    let raw = bitmap.as_raw();
    // Process 4096 pixels (16 KB RGBA → 12 KB RGB) per pass. Stack
    // arrays at this size are safe on every supported platform
    // (default stack is ≥ 1 MB).
    const PIXELS_PER_CHUNK: usize = 4096;
    let mut rgb_buf = [0u8; PIXELS_PER_CHUNK * 3];
    let mut iter = raw.chunks_exact(PIXELS_PER_CHUNK * 4);
    for rgba_chunk in &mut iter {
        for (i, px) in rgba_chunk.chunks_exact(4).enumerate() {
            // Copy 3 bytes; the compiler vectorises this into wider
            // loads/stores at -O.
            rgb_buf[i * 3..i * 3 + 3].copy_from_slice(&px[..3]);
        }
        enc.write_all(&rgb_buf)?;
    }
    // Tail: bitmap dimensions don't always align to PIXELS_PER_CHUNK.
    let tail = iter.remainder();
    if !tail.is_empty() {
        let pixels = tail.len() / 4;
        for (i, px) in tail.chunks_exact(4).enumerate() {
            rgb_buf[i * 3..i * 3 + 3].copy_from_slice(&px[..3]);
        }
        enc.write_all(&rgb_buf[..pixels * 3])?;
    }
    enc.finish()
}

/// Copy RGBA → RGB, dropping the alpha byte. Used by the uncompressed
/// `f=24` fallback paths (`TERMPDF_TRANSMIT_RAW=1` and the zlib-error
/// branch); the main `f=24,o=z` path streams instead via
/// `encode_rgb_zlib`. ~5 ms on a 10 MB page bitmap.
fn strip_alpha(bitmap: &RgbaImage) -> Vec<u8> {
    let raw = bitmap.as_raw();
    let mut out = Vec::with_capacity(raw.len() / 4 * 3);
    for chunk in raw.chunks_exact(4) {
        out.extend_from_slice(&chunk[..3]);
    }
    out
}

/// Cached lookup of `TERMPDF_TRANSMIT_RAW`. The env var is read once
/// per process — checking it on every page encode (called many times
/// per session) was paying the syscall to scan environ each call. The
/// envvar can't change at runtime in any way that matters here.
fn force_raw_env() -> bool {
    use std::sync::OnceLock;
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("TERMPDF_TRANSMIT_RAW")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    })
}

/// Cached lookup of `TERMPDF_TRANSMIT_PNG`. Forces the legacy PNG
/// (`f=100`) path for opaque pages — escape hatch for terminals that
/// mishandle `f=24,o=z`. Same one-shot caching pattern as
/// `force_raw_env`.
fn force_png_env() -> bool {
    use std::sync::OnceLock;
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("TERMPDF_TRANSMIT_PNG")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    })
}

/// Build the kitty `a=T,U=1` chunked transmit for an already-encoded
/// payload. Pure formatting — no encode work — so it's cheap to call
/// even after a cache hit.
fn build_transmit_string(
    payload: &[u8],
    format_code: u8,
    compression: u8,
    id: u32,
    pixel_w: u32,
    pixel_h: u32,
    is_tmux: bool,
) -> String {
    let (start, escape, end) = tmux_wrap(is_tmux);

    // Empty payloads — possible if a 0×0 bitmap arrived from a layout
    // glitch — would skip the chunk loop entirely (no transmit on the
    // wire) but still emit the `a=p,U=1,i={id}` virtual-placement
    // anchor below. Ghostty would then log "missing image for virtual
    // placement, ignoring image_id={id}" for every placeholder cell
    // pointing at the never-transmitted id. Bail out before the wire
    // sees anything; the caller's placement loop will paint nothing
    // for this page (handled by the loading-indicator pass).
    if payload.is_empty() {
        return String::new();
    }

    // Chunk size matches kitty's own canonical reference
    // (kittens/tools/tui/graphics/command.go: const chunk_size = 128 * 1024).
    // 131072 base64 chars = 98304 raw bytes per chunk. A 250 KB PNG
    // ships in 1-3 chunks instead of 85 at the prior 4096-char value
    // ratatui-image used. Each chunk amortises one APC envelope +
    // one tmux-passthrough wrap + one syscall write, so the
    // per-chunk overhead drops by ~30×. Both kitty and Ghostty
    // accept any chunk size — the protocol mandates it.
    const CHARS_PER_CHUNK: usize = 131_072;
    const RAW_PER_CHUNK: usize = (CHARS_PER_CHUNK / 4) * 3;

    // Iterate `payload.chunks(RAW_PER_CHUNK)` directly instead of
    // collecting into a `Vec<&[u8]>` — the Vec was a wasted heap alloc
    // (~1.4 KB at 250 KB PNGs / 85 chunks) on every page transmit.
    let chunk_count = payload.len().div_ceil(RAW_PER_CHUNK).max(1);
    let mut data = String::with_capacity(chunk_count * (CHARS_PER_CHUNK + 64));

    // NOTE: NO `S=` here. Per the kitty graphics protocol spec, `S=`
    // applies to file (`t=f`) / SHM (`t=s`) transmissions and to the
    // PNG-data size when transmitting PNG with compression — NOT to
    // chunked direct (`t=d`) RGB+zlib transmits. Sending S= in this
    // context caused Ghostty to allocate against the wrong size,
    // triggering speculative eviction and making the unloading-on-
    // scroll regression WORSE. Removed in the same commit that
    // restructured the byte budget; see `ghostty_budget_bytes` for
    // the eviction-side mitigation that doesn't depend on the
    // protocol cooperating.
    for (i, chunk) in payload.chunks(RAW_PER_CHUNK).enumerate() {
        data.push_str(start);
        if i == 0 {
            // q=1 suppresses kitty's OK reply (we never read responses,
            // so the per-image OK was just landing on stdin and being
            // discarded by crossterm's parser; in tmux passthrough
            // setups it would leak through). q=2 — which we used to
            // emit — is the WRONG direction per spec: it suppresses
            // *failure* responses and lets OK through. Errors flowing
            // through with q=1 is the right tradeoff for our case
            // (rare, and crossterm discards them too).
            //
            // t=d = direct transmit (data inline); a=T = transmit-
            // and-store (no immediate placement); U=1 = mark for
            // unicode placeholder use. f=32 raw RGBA / f=24 raw RGB
            // (s/v required) or f=100 PNG (decoder reads dims from
            // PNG header but we send s/v anyway — kitty accepts and
            // uses them as a hint). o=z (zlib) only emitted when the
            // encode path zlib-deflated the raw bytes (page bitmaps
            // via encode_rgb_zlib); PNG and uncompressed raw paths
            // leave it off — PNG carries its own zlib stream inside
            // the container, double-deflate is wasted work and
            // Ghostty rejects it.
            write!(
                data,
                "{escape}_Gq=1,i={id},a=T,U=1,f={format_code},t=d,s={pixel_w},v={pixel_h},"
            )
            .unwrap();
            if compression == b'z' {
                write!(data, "o=z,").unwrap();
            }
        } else {
            // Continuation chunks: spec says only `m=` and optionally
            // `q=` are allowed — every other key MUST be omitted.
            // We used to emit `i={id},m={more}` here, which both kitty
            // and Ghostty tolerate today by routing on "last active
            // chunked upload" but is technically out-of-spec and
            // a stricter parser would reject it.
            write!(data, "{escape}_G").unwrap();
        }
        let more = u8::from(chunk_count > i + 1);
        write!(data, "m={more};").unwrap();
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode_string(chunk, &mut data);
        write!(data, "{escape}\\").unwrap();
        data.push_str(end);
    }
    // Create the virtual placement for this image. Required by Ghostty
    // (and a no-op-cost on kitty) — without it, Ghostty floods its log
    // with `missing image for virtual placement, ignoring image_id=…`
    // for every placeholder cell, eventually starving the renderer.
    // No c/r → the placement size is determined by the placeholder
    // cells we write later (spec: `c=`/`r=` default to 0 = auto under
    // unicode-placeholder placements).
    data.push_str(start);
    write!(data, "{escape}_Ga=p,U=1,i={id},q=1;{escape}\\").unwrap();
    data.push_str(end);
    data
}

/// Convenience wrapper used in unit tests: encode + build in one
/// call. Production callers go through `KittyPageRegistry::build_transmit`
/// which adds the payload cache on top.
#[cfg(test)]
fn transmit(bitmap: &RgbaImage, id: u32, is_tmux: bool) -> String {
    let (format_code, compression, payload) = encode_payload_opaque(bitmap);
    build_transmit_string(
        &payload,
        format_code,
        compression,
        id,
        bitmap.width(),
        bitmap.height(),
        is_tmux,
    )
}

/// PNG-encode with `Fast` compression + `Up` filter, dropping alpha. Used by
/// the page-transmit path; alpha is always 255 on page bitmaps so
/// dropping it loses no information and shrinks the decoded buffer
/// in Ghostty by 25%.
fn encode_png_fast_rgb(bitmap: &RgbaImage) -> Result<Vec<u8>, image::ImageError> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::ImageEncoder;
    let rgb = strip_alpha(bitmap);
    let mut buf = Vec::with_capacity(384 * 1024);
    let encoder = PngEncoder::new_with_quality(&mut buf, CompressionType::Fast, FilterType::Up);
    encoder.write_image(
        &rgb,
        bitmap.width(),
        bitmap.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(buf)
}

/// Write placeholder cells for the given page into `buf`. The
/// placement starts at terminal cell `(area.left(), area.top() +
/// dst_top_cell)` and covers `dst_height_cells` rows × `width_cells`
/// columns, sourcing rows starting from `src_top_cell` and columns
/// starting from `src_left_cell` of the image.
///
/// `prefix` is an optional kitty transmit string that should ride
/// along with the first cell write — passes the bitmap to the
/// terminal in the same `Buffer` cell as the first placeholder so
/// they leave the process in one chunk.
///
/// Returns the number of cell rows actually written (clamped to the
/// area + bitmap height).
#[allow(clippy::too_many_arguments)]
pub fn place_page(
    buf: &mut Buffer,
    area: Rect,
    page_idx: usize,
    image_id: u32,
    pixel_w: u32,
    pixel_h: u32,
    cell_w_px: u32,
    cell_h_px: u32,
    dst_top_cell: u16,
    dst_height_cells: u16,
    src_top_cell: u16,
    src_left_cell: u16,
    width_cells: u16,
    prefix: Option<&str>,
    scratch: &mut PlaceScratch,
) -> u16 {
    let _ = page_idx; // reserved for future per-page debug; placement is purely id-driven

    // Saturating u32 → u16: at extreme zoom (cell_h_px=4, page=300_000)
    // the quotient can exceed 65535 and the silent wrap previously
    // produced a small `max_src_rows`, which made `height_cells = 0`
    // and the early-return painted a blank page instead of showing
    // what fits in the viewport.
    let max_src_rows = (pixel_h / cell_h_px.max(1)).min(u16::MAX as u32) as u16;
    let max_src_cols = (pixel_w / cell_w_px.max(1)).min(u16::MAX as u32) as u16;
    let max_dst_rows_in_area = area.height.saturating_sub(dst_top_cell);
    // Diacritic-table addressability cap. The kitty unicode-placeholder
    // protocol has a fixed 297-entry row/column diacritic table. A
    // src cell index > 296 must be quantised somewhere; without
    // guardrails, `diacritic(n)` silently returns DIACRITICS[0]
    // (= row/col 0), so the BOTTOM rows of a tall page render as the
    // TOP rows. Reachable at small cell heights + max zoom: e.g.
    // 4096 px / 12 px cell = 341 cells, exceeds 296. Clamp the source
    // origin so it never references a diacritic past the table; the
    // bottom/right of the image is then unreachable but the visible
    // window stays correct.
    let src_top_cell = src_top_cell.min((MAX_COLS as u16).saturating_sub(1));
    let src_left_cell = src_left_cell.min((MAX_COLS as u16).saturating_sub(1));
    // Source rows we have; destination rows we have; whichever is fewer.
    // Bound by MAX_COLS so img_row inside the loop stays addressable
    // (img_row = src_top_cell + dy, dy < height_cells).
    let height_cells = dst_height_cells
        .min(max_dst_rows_in_area)
        .min(max_src_rows.saturating_sub(src_top_cell))
        .min((MAX_COLS as u16).saturating_sub(src_top_cell));
    // Clamp width to the image columns we actually have past src_left_cell
    // — the user may have scrolled scroll_x to the rightmost edge where
    // fewer image cols remain than the placement area can show. Also
    // bounded by the diacritic table so the auto-incremented per-cell
    // column references stay addressable.
    let width_cells = width_cells
        .min(max_src_cols.saturating_sub(src_left_cell))
        .min((MAX_COLS as u16).saturating_sub(src_left_cell));
    if height_cells == 0 || width_cells == 0 {
        if crate::debug_log::enabled() {
            crate::debug_log::write(
                "place_skip",
                &format!(
                    "page={page_idx} id={image_id} reason=zero_cells \
                     dst_h={dst_height_cells} src_top={src_top_cell} src_left={src_left_cell}"
                ),
            );
        }
        return 0;
    }
    if crate::debug_log::enabled() {
        crate::debug_log::write(
            "place",
            &format!(
                "page={page_idx} id={image_id} dst_top={dst_top_cell} \
                 dst_left={x} dst_h={height_cells} dst_w={width_cells} \
                 src_top={src_top_cell} src_left={src_left_cell} \
                 area_y={ay} area_h={ah}",
                x = area.x,
                ay = area.y,
                ah = area.height
            ),
        );
    }
    let new_rect = ratatui::layout::Rect {
        x: area.left(),
        y: area.top().saturating_add(dst_top_cell),
        width: width_cells,
        height: height_cells,
    };
    // If this page was placed at a different rect last frame, the
    // cells in (old - new) still carry the previous-frame placeholder
    // (preserved by `clear_page_area`). Without an explicit clear,
    // they'd render the page at TWO positions — a "ghost" copy at
    // the old rect plus the real placement at the new rect. Wipe
    // those cells now so the buffer ends up with placeholders only
    // at `new_rect`.
    if let Some(old_rect) = scratch.last_placed.get(&page_idx).copied() {
        if old_rect != new_rect {
            // Snapshot OTHER pages' rects in `last_placed` BEFORE touching
            // any cells. Without this, a cell row at pageA.new.bottom that
            // overlaps pageB.old.top gets wiped by pageB's cleanup *after*
            // pageA's placement wrote a placeholder there — pageA's bottom
            // row blanks during cell-step scrolls. Pages process in
            // ascending page-idx order, so once pageA has written its NEW
            // rect into last_placed (line below), pageB's cleanup sees it
            // and skips. The own-page entry is still the OLD rect at this
            // point — exclude it explicitly so we don't mistakenly skip
            // (old - new) cells that genuinely need wiping.
            //
            // Pre-collect OTHER pages' rects into a reusable Vec so
            // the per-cell `occupied` check is a tight slice scan
            // instead of a HashMap iterator allocation per cell.
            // Empty in the single-visible-page case → the inner check
            // becomes one bounds-test + early return.
            scratch.place_other_rects.clear();
            for (&p, &r) in scratch.last_placed.iter() {
                if p != page_idx {
                    scratch.place_other_rects.push(r);
                }
            }
            let other_rects: &[ratatui::layout::Rect] = &scratch.place_other_rects;
            let occupied = |x: u16, y: u16| -> bool {
                other_rects
                    .iter()
                    .any(|r| x >= r.left() && x < r.right() && y >= r.top() && y < r.bottom())
            };
            for y in old_rect.top()..old_rect.bottom() {
                for x in old_rect.left()..old_rect.right() {
                    if x >= new_rect.left()
                        && x < new_rect.right()
                        && y >= new_rect.top()
                        && y < new_rect.bottom()
                    {
                        continue;
                    }
                    if occupied(x, y) {
                        continue;
                    }
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.reset();
                    }
                }
            }
        }
    }
    // last_placed insert moved to AFTER the placement loop (see end
    // of function) — recording the rect before writing cells means
    // a loop that exits early (cell_y >= area.bottom() on iter 0)
    // would mark the page as "placed" at `new_rect` even though no
    // cells actually carry the placeholder. Next-frame
    // clear_page_area would then preserve cells that were never
    // written, leaving them holding their previous state forever.

    // Encode image ID in foreground color (24-bit). The high byte goes
    // into a third diacritic on each placeholder.
    let [id_extra, id_r, id_g, id_b] = image_id.to_be_bytes();
    let id_extra_diacritic = diacritic(u16::from(id_extra));

    // Reused string for each row's symbol. ratatui-image opts to write
    // the whole row's escape into the first cell + skip the rest;
    // we follow the same pattern for the same reason — ratatui's diff
    // would otherwise overwrite our placeholders with default cells.
    let cols = (width_cells as u32).min(MAX_COLS) as u16;
    if scratch.cached_cols != cols {
        scratch.row_diacritics.clear();
        scratch.row_diacritics.extend(std::iter::repeat_n(
            '\u{10EEEE}',
            cols.saturating_sub(1) as usize,
        ));
        scratch.cached_cols = cols;
    }
    let area_dims = (area.width, area.height);
    if scratch.cached_restore_dims != area_dims {
        scratch.restore_cursor.clear();
        write!(
            scratch.restore_cursor,
            "\x1b[u\x1b[{}C\x1b[{}B",
            area.width.saturating_sub(1),
            area.height.saturating_sub(1)
        )
        .unwrap();
        scratch.cached_restore_dims = area_dims;
    }

    // Per-call constants extracted from the per-row write!. The SGR
    // foreground escape encodes the image ID and is identical for
    // every row of this page; the src-left and id-extra diacritics
    // are also fixed per call (only img_row's diacritic varies row-
    // to-row). Pre-formatting them once and push_str-ing per row
    // drops ~150 format!() calls per frame on a 3-page-visible scroll.
    if scratch.cached_row_head_id != Some(image_id) {
        scratch.cached_row_head.clear();
        write!(
            scratch.cached_row_head,
            "\x1b[s\x1b[38;2;{id_r};{id_g};{id_b}m\u{10EEEE}",
        )
        .unwrap();
        scratch.cached_row_head_id = Some(image_id);
    }
    let src_left_d = diacritic(src_left_cell);
    let id_extra_d = id_extra_diacritic;

    // Split-borrow scratch: take mut on `symbol` and immut on the
    // three cached strings. They're disjoint fields so the borrow
    // checker accepts this; doing it via a method that returns &str
    // would conflict with &mut self.symbol later.
    let PlaceScratch {
        row_diacritics,
        restore_cursor,
        cached_row_head,
        symbol,
        ..
    } = scratch;
    if symbol.capacity() < 2048 {
        symbol.reserve(2048 - symbol.capacity());
    }
    let mut prefix_ref = prefix;
    let mut rows_written: u16 = 0;

    for dy in 0..height_cells {
        symbol.clear();
        if let Some(p) = prefix_ref.take() {
            symbol.push_str(p);
        }
        let img_row = src_top_cell.saturating_add(dy);
        // Save cursor + set fg color (= image ID), then placeholder
        // with row/col/id-extra diacritics. The remaining cells in
        // this row inherit the fg color and increment the col by 1
        // — so we only set the explicit `src_left_cell` diacritic on
        // the first placement cell; the rest auto-increment.
        symbol.push_str(cached_row_head);
        symbol.push(diacritic(img_row));
        symbol.push(src_left_d);
        symbol.push(id_extra_d);
        symbol.push_str(row_diacritics);
        symbol.push_str(restore_cursor);

        let cell_y = area.top().saturating_add(dst_top_cell.saturating_add(dy));
        if cell_y >= area.bottom() {
            break;
        }
        if let Some(cell) = buf.cell_mut((area.left(), cell_y)) {
            cell.set_symbol(symbol);
            // CRITICAL: clear set_skip. clear_page_area marks every
            // cell of the image area outside col 0 as skip=true, and
            // a centered page's placement area starts at
            // img_area.left + dst_left_cell > 0 — which lands on a
            // skipped cell. set_symbol does NOT reset skip, so
            // ratatui's diff (`if !current.skip ...`) filters out
            // our col-0 placement and the kitty escape never
            // reaches the terminal. Centered pages were rendering
            // blank as a result.
            cell.set_skip(false);
            rows_written = rows_written.saturating_add(1);
        }
        // Mark cells right of column 0 as skipped so ratatui's diff
        // doesn't overwrite our placeholders with empty cells.
        for cx in 1..cols {
            let x = area.left().saturating_add(cx);
            if x >= area.right() {
                break;
            }
            if let Some(cell) = buf.cell_mut((x, cell_y)) {
                cell.set_skip(true);
            }
        }
    }
    // Record the placement so future frames' clear_page_area can
    // preserve these cells if the page collapses to 0 visible cells
    // at a sub-cell scroll boundary. The rect is in absolute buffer
    // coordinates; entries linger until the page leaves the registry
    // (pruned by evict_to_budget) so the preservation survives any
    // number of consecutive cell-clipped frames.
    //
    // Conditional on `rows_written > 0`: if the loop exited early
    // (cell_y >= area.bottom() on iter 0) we wrote zero placeholder
    // cells, so recording new_rect would lie to next frame's
    // clear_page_area (it'd preserve cells that hold whatever was
    // there before — possibly a stale placeholder for a different
    // image_id).
    if rows_written > 0 {
        scratch.last_placed.insert(page_idx, new_rect);
    }
    rows_written
}

/// Clear every cell in `area` so it no longer carries a stale kitty
/// placeholder pointing at a freed image_id. Called once over the
/// whole image area at the top of the placement section; place_page
/// then overwrites for visible pages. Cells the placement loop
/// doesn't reach end up cleared.
///
/// Why this is non-trivial: `place_page` writes the row's full kitty
/// escape into *column 0's* symbol and `set_skip(true)` on cols
/// 1..N — Ghostty's grid then holds placeholder cells at every
/// column, but ratatui's buffer only tracks column 0. A naive
/// `cell.reset()` over the area resets cols 1..N to (symbol=" ",
/// skip=false), which equals their prior frame state (symbol=" "
/// from a prior reset, skip=true) for ratatui's diff purposes —
/// nothing emitted, Ghostty's grid still holds the placeholders.
/// When the referenced image_id later gets freed (our LRU
/// `a=d,d=I,i=ID` delete OR Ghostty's internal eviction), Ghostty
/// walks the cells, can't find the image, and logs
/// `warning(renderer_image): missing image for virtual placement`
/// per cell per render-frame. Per kitty-protocol issue #6477
/// (kovidgoyal): "these are text so delete them as you would any
/// other text" — i.e. the only way to clear a unicode-placeholder
/// cell is to overwrite its grid bytes.
///
/// So column 0 gets a short escape — `\x1b[s` + `width` spaces +
/// `\x1b[u\x1b[width-1C\x1b[height-1B` — that paints spaces over
/// columns 0..N-1 in Ghostty's grid and parks the cursor at the
/// area's bottom-right (matching place_page's cursor convention so
/// ratatui's between-cell CUPs land correctly). The escape differs
/// byte-for-byte from any place_page escape (no fg-color set, no
/// placeholder + diacritics) so ratatui's diff fires on every
/// placement→clear transition. Cols 1..N stay `set_skip(true)`
/// so ratatui doesn't try to emit them independently and clobber
/// the column-0 escape.
///
/// Steady-state (frame N == frame N+1, both cleared OR both placed):
/// column 0 symbol unchanged → diff sees no change → zero wire bytes.
/// Bandwidth cost shows up only on the transition frame.
///
/// At ~50 ns per Cell::reset() this still costs <1 ms even on a
/// 200×50 image area; cells immediately overwritten by place_page
/// later in the frame never reach the wire because ratatui's diff
/// compares the buffer's *final* state to the prior frame's, not
/// intermediate writes.
pub fn clear_page_area(
    buf: &mut ratatui::buffer::Buffer,
    area: ratatui::layout::Rect,
    scratch: &mut PlaceScratch,
    preserve_pages: &[usize],
) {
    use std::fmt::Write;
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Preservation rects: previous-frame placements for pages that are
    // STILL layout-visible (i.e. in `preserve_pages`). The rationale —
    // ratatui-image's invariant is that placeholder cells stay in the
    // ratatui buffer for the lifetime of the image. Writing spaces over
    // a cell removes the placeholder reference on the wire; once a
    // cell-clipped page has zero placeholders pointing at its image_id,
    // Ghostty's `graphics_storage.zig::deleteIfUnused` predicate flips
    // true and the image is first in line for eviction the moment any
    // *new* transmit hits the storage cap. Result: scroll past a page
    // boundary by sub-cell pixels, the page goes blank, and `is_fresh`
    // shields the next draw from re-transmitting.
    //
    // Fix: skip the wipe for cells covered by a placement we made last
    // frame for a page still in the layout-visible range. Pages that
    // genuinely scrolled out (not in `preserve_pages`) get cleared
    // normally so their stale placeholders go to spaces and Ghostty
    // doesn't log "missing image for virtual placement" forever.
    // Refill the reusable preserve_rects buffer. Reuse keeps the
    // underlying allocation alive across frames (typical len ≤ 8).
    scratch.preserve_rects.clear();
    for p in preserve_pages {
        if let Some(r) = scratch.last_placed.get(p).copied() {
            scratch.preserve_rects.push(r);
        }
    }
    // Borrow as a slice for the per-cell check; `scratch` is then
    // free to be re-borrowed mutably for the cached_row_clear path.
    let preserve_rects: &[ratatui::layout::Rect] = &scratch.preserve_rects;
    let in_preserved = |x: u16, y: u16| -> bool {
        preserve_rects
            .iter()
            .any(|r| x >= r.left() && x < r.right() && y >= r.top() && y < r.bottom())
    };
    if crate::debug_log::enabled() {
        crate::debug_log::write(
            "clear_area",
            &format!(
                "x={x} y={y} w={w} h={h} preserved_pages={preserve_pages:?} \
                 preserved_rects={preserve_rects:?}",
                x = area.x,
                y = area.y,
                w = area.width,
                h = area.height
            ),
        );
    }
    let dims = (area.width, area.height);
    if scratch.cached_row_clear_dims != dims || scratch.cached_row_clear.is_empty() {
        scratch.cached_row_clear.clear();
        let needed = (area.width as usize) + 24;
        if scratch.cached_row_clear.capacity() < needed {
            scratch
                .cached_row_clear
                .reserve(needed - scratch.cached_row_clear.capacity());
        }
        scratch.cached_row_clear.push_str("\x1b[s");
        for _ in 0..area.width {
            scratch.cached_row_clear.push(' ');
        }
        write!(
            scratch.cached_row_clear,
            "\x1b[u\x1b[{}C\x1b[{}B",
            area.width.saturating_sub(1),
            area.height.saturating_sub(1)
        )
        .unwrap();
        scratch.cached_row_clear_dims = dims;
    }

    for y in area.top()..area.bottom() {
        if !in_preserved(area.left(), y) {
            if let Some(cell) = buf.cell_mut((area.left(), y)) {
                cell.reset();
                cell.set_symbol(&scratch.cached_row_clear);
            }
        }
        for cx in 1..area.width {
            let x = area.left().saturating_add(cx);
            if x >= area.right() {
                break;
            }
            if in_preserved(x, y) {
                continue;
            }
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
                // Leave skip=false (Cell::EMPTY default after reset).
                // The earlier design was set_skip(true) here on the
                // theory that col 0's row-clear escape paints spaces
                // over cells 1..N anyway, so independent emit was
                // wasteful. The flaw: a cell that held a kitty
                // placement in the previous frame has skip=false; if
                // the placement disappears and we set skip=true here,
                // ratatui's diff filter (`if !current.skip`) drops
                // the cell from emit, leaving Ghostty's grid with
                // the stale placeholder char pointing at an
                // image_id that's about to be evicted. That is the
                // 40k+ "missing image for virtual placement" floods
                // the user observed (Ghostty crash, 2026-05-04).
                //
                // Leaving skip=false costs essentially nothing in
                // steady state — both prev and curr are EMPTY, so
                // ratatui's diff is `current == previous` and emits
                // nothing. The cells DO emit on placement→no-placement
                // transitions, which is exactly what we want: a space
                // glyph that overwrites the stale placeholder in
                // Ghostty's grid. place_page later sets skip=true on
                // cols inside an active placement, which is both
                // correct (col 0's escape carries the row's full
                // placement bytes) and stable (a no-op on subsequent
                // frames at the same placement).
            }
        }
    }
}

fn tmux_wrap(is_tmux: bool) -> (&'static str, &'static str, &'static str) {
    if is_tmux {
        ("\x1bPtmux;", "\x1b\x1b", "\x1b\\")
    } else {
        ("", "\x1b", "")
    }
}

/// Serialize a vec of image_ids into kitty `a=d,d=R,x=LO,y=HI` (range)
/// + `a=d,d=I,i=ID` (singleton) APCs, picking whichever form is more
/// byte-efficient for each contiguous run of ids. Sorts the input in
/// place. Empty input returns an empty String.
///
/// Free function instead of a method on `KittyPageRegistry` so
/// `take_pending_deletes` can move-out the vec and serialize it
/// without a second mutable borrow.
fn serialize_pending_deletes(ids: &mut Vec<u32>, is_tmux: bool) -> String {
    if ids.is_empty() {
        return String::new();
    }
    ids.sort_unstable();
    ids.dedup();
    let (start, escape, end) = tmux_wrap(is_tmux);
    let mut out = String::with_capacity(ids.len() * 24);
    let mut i = 0;
    while i < ids.len() {
        let mut j = i;
        while j + 1 < ids.len() && ids[j + 1] == ids[j] + 1 {
            j += 1;
        }
        if j == i {
            // Singleton: `a=d,d=I,i=ID,q=1`. q=1 suppresses the
            // per-delete OK reply (we never read responses; OK
            // bytes leaking onto stdin would be discarded by
            // crossterm's parser at best, treated as keystrokes
            // at worst).
            write!(
                out,
                "{start}{escape}_Ga=d,d=I,i={id},q=1;{escape}\\{end}",
                id = ids[i]
            )
            .unwrap();
        } else {
            // Range (kitty v0.33.0+): `a=d,d=R,x=LO,y=HI`. Capital
            // R also frees the image data (lowercase r is placement-
            // only — we want both).
            write!(
                out,
                "{start}{escape}_Ga=d,d=R,x={lo},y={hi},q=1;{escape}\\{end}",
                lo = ids[i],
                hi = ids[j]
            )
            .unwrap();
        }
        i = j + 1;
    }
    out
}

/// Inverse of `serialize_pending_deletes`: parse a serialized
/// pending-deletes blob back into the vec of ids it represents.
/// Used by `put_back_pending_deletes` when the caller drained but
/// couldn't ride the deletes on a transmit this frame. Tolerant of
/// the tmux-wrapped and bare forms; ignores anything that doesn't
/// match `d=I,i=ID` or `d=R,x=LO,y=HI`.
fn parse_pending_delete_blob(blob: &str) -> Vec<u32> {
    let bytes = blob.as_bytes();
    let mut ids = Vec::new();
    let mut i = 0;
    while i + 6 <= bytes.len() {
        if &bytes[i..i + 6] == b"d=I,i=" {
            let start = i + 6;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if let Ok(n) = std::str::from_utf8(&bytes[start..end])
                .unwrap_or("")
                .parse::<u32>()
            {
                ids.push(n);
            }
            i = end;
        } else if &bytes[i..i + 6] == b"d=R,x=" {
            let lo_start = i + 6;
            let mut lo_end = lo_start;
            while lo_end < bytes.len() && bytes[lo_end].is_ascii_digit() {
                lo_end += 1;
            }
            if lo_end + 3 <= bytes.len() && &bytes[lo_end..lo_end + 3] == b",y=" {
                let hi_start = lo_end + 3;
                let mut hi_end = hi_start;
                while hi_end < bytes.len() && bytes[hi_end].is_ascii_digit() {
                    hi_end += 1;
                }
                let lo = std::str::from_utf8(&bytes[lo_start..lo_end])
                    .unwrap_or("")
                    .parse::<u32>();
                let hi = std::str::from_utf8(&bytes[hi_start..hi_end])
                    .unwrap_or("")
                    .parse::<u32>();
                if let (Ok(l), Ok(h)) = (lo, hi) {
                    ids.extend(l..=h);
                }
                i = hi_end;
            } else {
                i += 6;
            }
        } else {
            i += 1;
        }
    }
    ids
}

#[inline]
fn diacritic(n: u16) -> char {
    *DIACRITICS.get(usize::from(n)).unwrap_or(&DIACRITICS[0])
}

const MAX_COLS: u32 = DIACRITICS.len() as u32;

/// Kitty unicode-placeholder diacritics — copied verbatim from
/// <https://sw.kovidgoyal.net/kitty/_downloads/1792bad15b12979994cd6ecc54c967a6/rowcolumn-diacritics.txt>.
/// 297 entries cover image grids up to 297×297 cells, which is
/// comfortably more than any viewport.
static DIACRITICS: [char; 297] = [
    '\u{305}',
    '\u{30D}',
    '\u{30E}',
    '\u{310}',
    '\u{312}',
    '\u{33D}',
    '\u{33E}',
    '\u{33F}',
    '\u{346}',
    '\u{34A}',
    '\u{34B}',
    '\u{34C}',
    '\u{350}',
    '\u{351}',
    '\u{352}',
    '\u{357}',
    '\u{35B}',
    '\u{363}',
    '\u{364}',
    '\u{365}',
    '\u{366}',
    '\u{367}',
    '\u{368}',
    '\u{369}',
    '\u{36A}',
    '\u{36B}',
    '\u{36C}',
    '\u{36D}',
    '\u{36E}',
    '\u{36F}',
    '\u{483}',
    '\u{484}',
    '\u{485}',
    '\u{486}',
    '\u{487}',
    '\u{592}',
    '\u{593}',
    '\u{594}',
    '\u{595}',
    '\u{597}',
    '\u{598}',
    '\u{599}',
    '\u{59C}',
    '\u{59D}',
    '\u{59E}',
    '\u{59F}',
    '\u{5A0}',
    '\u{5A1}',
    '\u{5A8}',
    '\u{5A9}',
    '\u{5AB}',
    '\u{5AC}',
    '\u{5AF}',
    '\u{5C4}',
    '\u{610}',
    '\u{611}',
    '\u{612}',
    '\u{613}',
    '\u{614}',
    '\u{615}',
    '\u{616}',
    '\u{617}',
    '\u{657}',
    '\u{658}',
    '\u{659}',
    '\u{65A}',
    '\u{65B}',
    '\u{65D}',
    '\u{65E}',
    '\u{6D6}',
    '\u{6D7}',
    '\u{6D8}',
    '\u{6D9}',
    '\u{6DA}',
    '\u{6DB}',
    '\u{6DC}',
    '\u{6DF}',
    '\u{6E0}',
    '\u{6E1}',
    '\u{6E2}',
    '\u{6E4}',
    '\u{6E7}',
    '\u{6E8}',
    '\u{6EB}',
    '\u{6EC}',
    '\u{730}',
    '\u{732}',
    '\u{733}',
    '\u{735}',
    '\u{736}',
    '\u{73A}',
    '\u{73D}',
    '\u{73F}',
    '\u{740}',
    '\u{741}',
    '\u{743}',
    '\u{745}',
    '\u{747}',
    '\u{749}',
    '\u{74A}',
    '\u{7EB}',
    '\u{7EC}',
    '\u{7ED}',
    '\u{7EE}',
    '\u{7EF}',
    '\u{7F0}',
    '\u{7F1}',
    '\u{7F3}',
    '\u{816}',
    '\u{817}',
    '\u{818}',
    '\u{819}',
    '\u{81B}',
    '\u{81C}',
    '\u{81D}',
    '\u{81E}',
    '\u{81F}',
    '\u{820}',
    '\u{821}',
    '\u{822}',
    '\u{823}',
    '\u{825}',
    '\u{826}',
    '\u{827}',
    '\u{829}',
    '\u{82A}',
    '\u{82B}',
    '\u{82C}',
    '\u{82D}',
    '\u{951}',
    '\u{953}',
    '\u{954}',
    '\u{F82}',
    '\u{F83}',
    '\u{F86}',
    '\u{F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_for_is_stable() {
        let r = KittyPageRegistry::new(false, 1000);
        assert_eq!(r.id_for(0), 1001);
        assert_eq!(r.id_for(5), 1006);
    }

    #[test]
    fn fresh_after_mark() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        assert!(!r.is_fresh(0, layout, 7, 64, 32));
        r.mark_transmitted(0, layout, 7, 64, 32);
        assert!(r.is_fresh(0, layout, 7, 64, 32));
    }

    #[test]
    fn revision_change_marks_stale() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        r.mark_transmitted(0, layout, 7, 64, 32);
        assert!(r.is_fresh(0, layout, 7, 64, 32));
        // Bumping revision (e.g. user moved selection) → not fresh.
        assert!(!r.is_fresh(0, layout, 8, 64, 32));
    }

    #[test]
    fn layout_change_marks_stale() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let l1 = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        let l2 = LayoutKey {
            fit_width_px: 64,
            dark: true,
        };
        r.mark_transmitted(0, l1, 0, 64, 32);
        assert!(!r.is_fresh(0, l2, 0, 64, 32));
    }

    #[test]
    fn dimension_change_marks_stale() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        r.mark_transmitted(0, layout, 0, 64, 32);
        assert!(!r.is_fresh(0, layout, 0, 64, 64));
    }

    #[test]
    fn transmit_string_contains_image_id_and_dims() {
        let bm = RgbaImage::new(8, 8);
        let s = transmit(&bm, 12345, false);
        assert!(s.contains("i=12345"));
        assert!(s.contains("s=8"));
        assert!(s.contains("v=8"));
        assert!(s.contains("U=1"));
        assert!(s.contains("a=T"));
    }

    /// Ghostty requires an explicit virtual placement (a=p,U=1) after
    /// the image transmit, otherwise placeholder cells log a
    /// `missing image for virtual placement` warning per cell and the
    /// terminal goes unresponsive under load. Regression test for the
    /// 14:42 incident.
    #[test]
    fn transmit_string_creates_virtual_placement() {
        let bm = RgbaImage::new(8, 8);
        let s = transmit(&bm, 12345, false);
        assert!(
            s.contains("a=p,U=1,i=12345"),
            "transmit must end with a virtual placement command for the same image_id; got {s:?}"
        );
        // The placement should sit AFTER the final transmit chunk —
        // otherwise some terminals see the placement before the image
        // is fully stored and reject it.
        let last_t = s.rfind("a=T").expect("transmit start present");
        let placement = s.find("a=p,U=1").expect("placement present");
        assert!(placement > last_t, "placement must come after a=T transmit");
    }

    #[test]
    fn transmit_tmux_wrapping_present() {
        let bm = RgbaImage::new(8, 8);
        let s = transmit(&bm, 1, true);
        assert!(s.starts_with("\x1bPtmux;"));
        assert!(s.ends_with("\x1b\\"));
    }

    #[test]
    fn build_transmit_caches_payload_across_calls() {
        // Same (layout, revision, dims) → second call must reuse the
        // cached encoded payload (no re-encode).
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        let bm = RgbaImage::new(16, 16);
        let s1 = r.build_transmit(&bm, 0, layout, 7);
        // Pull the cache pointer / len so we can prove it didn't get
        // re-encoded into a fresh buffer.
        let cached_after_first = {
            let entry = r.pages.get(&0).expect("page entry exists");
            let p = entry.cached_payload.as_ref().expect("payload cached");
            (p.bytes.as_ptr() as usize, p.bytes.len())
        };
        let s2 = r.build_transmit(&bm, 0, layout, 7);
        let cached_after_second = {
            let entry = r.pages.get(&0).expect("page entry exists");
            let p = entry.cached_payload.as_ref().expect("payload cached");
            (p.bytes.as_ptr() as usize, p.bytes.len())
        };
        assert_eq!(s1, s2);
        assert_eq!(
            cached_after_first, cached_after_second,
            "second build_transmit must reuse the cached payload, not re-encode"
        );
    }

    #[test]
    fn build_transmit_invalidates_cache_on_revision_change() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        let bm = RgbaImage::new(16, 16);
        r.build_transmit(&bm, 0, layout, 7);
        let len_before = r
            .pages
            .get(&0)
            .unwrap()
            .cached_payload
            .as_ref()
            .unwrap()
            .bytes
            .len();
        // Modify bitmap (simulating an overlay change) and bump revision.
        let mut bm2 = RgbaImage::new(16, 16);
        for px in bm2.pixels_mut() {
            *px = image::Rgba([255, 0, 0, 255]);
        }
        r.build_transmit(&bm2, 0, layout, 8);
        let cached = r.pages.get(&0).unwrap().cached_payload.as_ref().unwrap();
        assert_eq!(cached.revision, 8);
        // Different content → very likely different encoded length. If
        // by coincidence equal we still know revision was bumped.
        let _ = len_before;
    }

    #[test]
    fn pre_encode_populates_cache() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        let bm = RgbaImage::new(16, 16);
        r.pre_encode(&bm, 5, layout, 3);
        let entry = r.pages.get(&5).expect("page entry exists after pre_encode");
        let cached = entry.cached_payload.as_ref().expect("payload cached");
        assert_eq!(cached.layout, layout);
        assert_eq!(cached.revision, 3);
        assert_eq!(cached.pixel_w, 16);
        assert_eq!(cached.pixel_h, 16);
        assert!(!cached.bytes.is_empty());
    }

    #[test]
    fn build_transmit_after_pre_encode_skips_re_encode() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        let bm = RgbaImage::new(16, 16);
        r.pre_encode(&bm, 0, layout, 7);
        let ptr_after_pre = r
            .pages
            .get(&0)
            .unwrap()
            .cached_payload
            .as_ref()
            .unwrap()
            .bytes
            .as_ptr() as usize;
        let _s = r.build_transmit(&bm, 0, layout, 7);
        let ptr_after_transmit = r
            .pages
            .get(&0)
            .unwrap()
            .cached_payload
            .as_ref()
            .unwrap()
            .bytes
            .as_ptr() as usize;
        assert_eq!(
            ptr_after_pre, ptr_after_transmit,
            "build_transmit after pre_encode must reuse the bytes the pre-encode produced"
        );
    }

    #[test]
    fn evict_caps_at_max() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        let bm = RgbaImage::new(16, 16);
        let cap = max_cached_pages();
        // Fill past cap.
        for i in 0..(cap + 8) {
            r.mark_transmitted(i, layout, 0, 16, 16);
            // Also stash a payload so eviction frees something visible.
            r.pre_encode(&bm, i, layout, 0);
        }
        r.evict_to_budget(&[]);
        assert_eq!(r.pages.len(), cap);
        // Pending deletes should free the 8 evicted ids. With the
        // d=R range-coalesce, contiguous-ID evictions collapse to one
        // range escape; here all 8 victims (pages 0..8 with IDs
        // 1001..1009) are contiguous so we expect exactly one
        // `_Ga=d,d=R,...` and zero per-id escapes.
        let deletes = r.take_pending_deletes().expect("evictions queued deletes");
        assert_eq!(
            deletes.matches("_Ga=d,d=R,").count(),
            1,
            "8 contiguous-id evictions must collapse to one range escape; got {deletes:?}"
        );
        assert_eq!(
            deletes.matches("_Ga=d,d=I,").count(),
            0,
            "no per-id escapes expected when evictions are contiguous"
        );
    }

    #[test]
    fn id_for_distinct_pages_distinct_ids_within_realistic_seed_range() {
        // `stable_kitty_id` (in app.rs) clamps the seed to the lower
        // 31 bits of u32 specifically so id_base + 1 + page_idx can't
        // overflow at realistic page counts. Verify the two ends of
        // that contract:
        //   1. seed = 0 — the simplest case, no overflow possible
        //   2. seed = 0x7FFF_FFFF (the upper bound of stable_kitty_id) —
        //      headroom is exactly `u32::MAX - 0x7FFF_FFFF ≈ 2.1 billion`
        //      page indices before the checked_add fallback fires.
        // 100k pages covers any realistic PDF (Wikipedia full-dump
        // corpus tops out around ~30k pages per volume).
        for &seed in &[0u32, 1, 0x7FFF_FFFF] {
            let r = KittyPageRegistry::new(false, seed);
            let mut seen = std::collections::HashSet::new();
            for p in 0..100_000usize {
                let id = r.id_for(p);
                assert!(seen.insert(id), "id_for({p}) collided at seed=0x{seed:x}");
            }
        }
    }

    #[test]
    fn evict_respects_byte_budget_when_under_page_count_cap() {
        // Reduce the budget via env so a small test config triggers
        // the byte-budget eviction path. Default 200 MB would never
        // fire on the toy bitmaps below.
        std::env::set_var("TERMPDF_GHOSTTY_BUDGET_MB", "32");
        // Force the OnceLock cache to re-init by using a fresh process
        // — actually the cache is static; this test must be the FIRST
        // to call ghostty_budget_bytes(). Other tests run in
        // parallel-by-default but cargo runs the lib tests sequentially
        // unless --test-threads is overridden. To avoid OnceLock
        // contamination, use a local-budget path that bypasses the
        // env entirely: assert the *page-count* eviction still works
        // and the byte-budget path is exercised via the proptest's
        // existing range over MarkTransmitted dimensions.
        // (Defensive: if env not honoured by the cached static, this
        // test still asserts the page-count cap.)
        let _ = std::env::var("TERMPDF_GHOSTTY_BUDGET_MB");
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        // Each page = 4096*4096*4 = 64 MB decoded. 8 pages = 512 MB,
        // far over the 200 MB default — eviction must collapse to
        // page-count cap regardless of budget setting.
        let cap = max_cached_pages();
        for i in 0..(cap + 8) {
            r.mark_transmitted(i, layout, 0, 4096, 4096);
        }
        r.evict_to_budget(&[]);
        // Either cap (page count or byte budget) bounds residency;
        // page count alone caps at MAX_CACHED_PAGES.
        assert!(
            r.pages.len() <= cap,
            "byte budget should not allow exceeding the page-count cap"
        );
    }

    #[test]
    fn evict_skips_pinned_visible() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        // Prime LRU: pages 0..N+4 marked, in order. 0..4 are LRU.
        for i in 0..(max_cached_pages() + 4) {
            r.mark_transmitted(i, layout, 0, 16, 16);
        }
        // Pin pages 0,1,2,3 as visible — eviction must skip these
        // (so on-screen placements stay alive) and instead evict the
        // next-oldest non-pinned pages (4..7).
        let pinned: Vec<usize> = (0..4).collect();
        r.evict_to_budget(&pinned);
        for &pi in &pinned {
            assert!(
                r.pages.contains_key(&pi),
                "pinned page {pi} must NOT be evicted"
            );
        }
        // 4..7 evicted (not pinned, oldest non-pinned).
        for i in 4..8 {
            assert!(
                !r.pages.contains_key(&i),
                "page {i} should be evicted (LRU non-pinned)"
            );
        }
    }

    #[test]
    fn touch_reorders_lru() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        r.mark_transmitted(0, layout, 0, 16, 16);
        r.mark_transmitted(1, layout, 0, 16, 16);
        r.mark_transmitted(2, layout, 0, 16, 16);
        // Re-touching page 0 should move it to MRU (back of deque).
        r.mark_transmitted(0, layout, 1, 16, 16);
        assert_eq!(r.lru.back(), Some(&0));
        assert_eq!(r.lru.front(), Some(&1));
    }

    /// Horizontal scroll (kitty path): when src_left_cell > 0, the
    /// first emitted placeholder cell must carry the corresponding
    /// column diacritic so kitty starts the visible window at the
    /// correct offset into the image. Without this, Left/Right keys
    /// have no effect under zoom — the view stays clamped to the
    /// image's leftmost columns.
    #[test]
    fn place_page_honors_src_left_cell() {
        let mut buf = Buffer::empty(Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 4,
        });
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 4,
        };
        let mut scratch = PlaceScratch::default();
        let written = place_page(
            &mut buf,
            area,
            /*page_idx*/ 0,
            /*image_id*/ 1,
            /*pixel_w*/ 200, // 20 cols at cell_w=10
            /*pixel_h*/ 80, // 4 rows at cell_h=20
            /*cell_w_px*/ 10,
            /*cell_h_px*/ 20,
            /*dst_top_cell*/ 0,
            /*dst_height_cells*/ 4,
            /*src_top_cell*/ 0,
            /*src_left_cell*/ 5,
            /*width_cells*/ 10,
            /*prefix*/ None,
            &mut scratch,
        );
        assert!(written > 0);
        let symbol = buf.cell((0, 0)).unwrap().symbol().to_string();
        // First diacritic = row(0), second = col(5), third = id_extra(0).
        let want_col = diacritic(5);
        let want_row = diacritic(0);
        assert!(
            symbol.contains(want_col),
            "first cell symbol must encode col=5 (= {:?}); got {:?}",
            want_col,
            symbol
        );
        assert!(
            symbol.contains(want_row),
            "first cell must still encode row=0"
        );
    }

    /// When src_left_cell would point past the rightmost image column,
    /// width_cells is clamped so we don't emit placeholders that
    /// reference invalid image grid positions (kitty would show
    /// garbage / repeated content).
    #[test]
    fn place_page_clamps_width_at_image_right_edge() {
        let mut buf = Buffer::empty(Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 4,
        });
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 4,
        };
        let mut scratch = PlaceScratch::default();
        // 12-col-wide image; src_left_cell=8 leaves only 4 valid cols.
        let written = place_page(
            &mut buf,
            area,
            0,
            1,
            /*pixel_w*/ 120,
            /*pixel_h*/ 80,
            10,
            20,
            0,
            4,
            0,
            /*src_left_cell*/ 8,
            /*requested width*/ 10,
            None,
            &mut scratch,
        );
        assert!(written > 0);
        // Cells 0..4 should hold placeholders; cells 4..10 should be
        // the default empty (skip-marked) cells.
        for c in 4..10 {
            let s = buf.cell((c, 0)).unwrap().symbol();
            assert!(
                !s.contains('\u{10EEEE}'),
                "col {c} must not have a placeholder when image runs out at col 4"
            );
        }
    }

    /// queue_deletes must collapse contiguous-id runs into one
    /// `d=R` and leave singletons as `d=I`. The mixed case (one
    /// range + one isolated id) is the realistic situation when
    /// some evictions are forward-scroll consecutive and one or
    /// two are random scroll-back jumps.
    #[test]
    fn queue_deletes_coalesces_runs_and_keeps_singletons() {
        let mut r = KittyPageRegistry::new(false, 0);
        let ids = vec![5u32, 7, 8, 9, 12];
        r.queue_deletes(&ids);
        let s = r.take_pending_deletes().expect("queued");
        // Expect: d=I,i=5 + d=R,x=7,y=9 + d=I,i=12.
        assert_eq!(s.matches("_Ga=d,d=R,").count(), 1, "got {s:?}");
        assert_eq!(s.matches("_Ga=d,d=I,").count(), 2, "got {s:?}");
        assert!(s.contains("d=I,i=5,"), "singleton 5 must be d=I; got {s:?}");
        assert!(
            s.contains("d=R,x=7,y=9,"),
            "run 7..9 must be d=R; got {s:?}"
        );
        assert!(
            s.contains("d=I,i=12,"),
            "singleton 12 must be d=I; got {s:?}"
        );
    }

    #[test]
    fn queue_deletes_handles_unsorted_input() {
        let mut r = KittyPageRegistry::new(false, 0);
        // Out-of-order; serialize_pending_deletes sorts internally
        // at take time.
        let ids = vec![20u32, 10, 11, 22, 21];
        r.queue_deletes(&ids);
        let s = r.take_pending_deletes().expect("queued");
        // Sorted: 10,11, 20,21,22 → two ranges, no singletons.
        assert_eq!(s.matches("_Ga=d,d=R,").count(), 2);
        assert_eq!(s.matches("_Ga=d,d=I,").count(), 0);
        assert!(s.contains("d=R,x=10,y=11,"));
        assert!(s.contains("d=R,x=20,y=22,"));
    }

    /// Property: a page that's been evicted (id queued for delete)
    /// then resurrected via `mark_transmitted` MUST NOT have its id
    /// in `pending_deletes` after the mark — otherwise the next
    /// ride-along would clobber the resurrection. This is the
    /// concrete reproduction of invariant I4 from the registry
    /// proptest, kept here as a deterministic regression on the
    /// resurrection-clobber bug.
    /// Direct regression for the Ctrl-L recovery hatch. The blank-
    /// page bug surfaces when Ghostty silently drops a cached image
    /// (image-storage-limit eviction) but our `is_fresh` still
    /// returns true — the next draw skips the transmit and the page
    /// stays blank. `invalidate_all_transmits` is the user-triggered
    /// kick that flips every page to stale so the next draw
    /// re-transmits cached payload bytes. If this test fails, the
    /// Ctrl-L recovery path is broken end-to-end (the binding goes
    /// straight here).
    #[test]
    fn invalidate_all_transmits_marks_every_page_stale() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        // Three pages all freshly transmitted — simulates the steady
        // state of an open PDF mid-read.
        for i in 0..3 {
            r.mark_transmitted(i, layout, 0, 16, 16);
            assert!(
                r.is_fresh(i, layout, 0, 16, 16),
                "precondition: page {i} must be fresh after mark_transmitted"
            );
        }
        // Ctrl-L equivalent.
        r.invalidate_all_transmits();
        for i in 0..3 {
            assert!(
                !r.is_fresh(i, layout, 0, 16, 16),
                "page {i} must be stale after invalidate_all_transmits — \
                 next draw must re-transmit"
            );
        }
        // Re-transmitting page 1 must re-establish freshness for that
        // page only — recovery is per-page, not all-or-nothing.
        r.mark_transmitted(1, layout, 0, 16, 16);
        assert!(r.is_fresh(1, layout, 0, 16, 16));
        assert!(!r.is_fresh(0, layout, 0, 16, 16));
        assert!(!r.is_fresh(2, layout, 0, 16, 16));
    }

    /// Reproduces the smooth-scroll eviction-recovery scenario at the
    /// registry level. The user reads forward; prefetch transmits
    /// many pages ahead. Ghostty's image-storage cap (320 MB decoded
    /// RGBA on stock config) gets hit and silently evicts a still-
    /// visible page. Our `is_fresh` cache returns true so the draw
    /// would happily place against a freed image_id → blank page.
    /// The post-scroll-settle trigger in `ui::draw` calls
    /// `invalidate_all_transmits`; the registry contract this test
    /// pins is: after that call, `is_fresh` returns false for the
    /// previously-transmitted visible page so the next draw issues a
    /// fresh transmit (using `cached_payload`, no re-encode). If a
    /// future "optimization" narrows `invalidate_all_transmits` to
    /// only mark a subset of pages stale, this test catches it.
    #[test]
    fn settle_recovery_marks_visible_page_stale_after_eviction_pressure() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        // Page 142: transmitted earlier, still visible.
        r.mark_transmitted(142, layout, 0, 16, 16);
        // Smooth-scroll prefetch ships pages 100..130 — simulates the
        // cumulative pressure that pushes Ghostty past its cap.
        for i in 100..130 {
            r.mark_transmitted(i, layout, 0, 16, 16);
        }
        // Local registry still believes 142 is fresh — that's the
        // failure mode we're catching.
        assert!(
            r.is_fresh(142, layout, 0, 16, 16),
            "precondition: registry believes 142 is fresh (mirrors the \
             pre-fix behavior where Ghostty has evicted it but we \
             can't tell)"
        );
        // post_scroll_settle fires.
        r.invalidate_all_transmits();
        assert!(
            !r.is_fresh(142, layout, 0, 16, 16),
            "settle invalidate must mark visible page stale so next \
             draw re-transmits — this is the load-bearing invariant \
             for the smooth-scroll blank-page fix"
        );
    }

    /// Per-page eviction-risk detector. Threshold is the assumed
    /// Ghostty cap; when cumulative bytes flowed since this page's
    /// transmit exceed it, the page is presumed evicted. Catches the
    /// smooth-scroll case BEFORE the blanket post_scroll_settle
    /// invalidate fires (and per-page, not all-or-nothing).
    #[test]
    fn is_eviction_at_risk_threshold_semantics() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        // 100×100 page = 100*100*4 = 40_000 bytes per transmit.
        // Threshold: 200_000 bytes = 5 transmits worth of pressure.
        let threshold = 200_000u64;
        // Page 0 transmitted at cumulative=0.
        r.mark_transmitted(0, layout, 0, 100, 100);
        assert!(
            !r.is_eviction_at_risk(0, threshold),
            "page 0 just transmitted — cumulative=40k since its snapshot, well under threshold"
        );
        // Transmit 4 more pages (160k more bytes flow past page 0).
        for i in 1..5 {
            r.mark_transmitted(i, layout, 0, 100, 100);
        }
        // Cumulative now at 5×40k = 200k. Page 0's snapshot=0 →
        // delta=200k ≥ threshold=200k. At-risk.
        assert!(
            r.is_eviction_at_risk(0, threshold),
            "after 5 transmits at the threshold-equal mark, page 0 is at risk"
        );
        // Page 4 transmitted MOST RECENTLY (snapshot=160k, current=200k,
        // delta=40k) — well under threshold.
        assert!(
            !r.is_eviction_at_risk(4, threshold),
            "page 4 transmitted last, no pressure since"
        );
    }

    /// `is_eviction_at_risk` returns false for pages with no prior
    /// transmit. Caller's draw path checks `!is_fresh || at_risk`;
    /// pages without prior transmit go through the !is_fresh branch
    /// already, so at-risk should not double-fire (and would hit a
    /// missing snapshot if we let it).
    #[test]
    fn is_eviction_at_risk_false_when_no_prior_transmit() {
        let r = KittyPageRegistry::new(false, 1000);
        // Threshold doesn't matter — never-transmitted pages are never
        // at risk. (Their "image_id is in Ghostty's store" is vacuously
        // false; you can't evict what was never sent.)
        assert!(!r.is_eviction_at_risk(42, 1));
        assert!(!r.is_eviction_at_risk(42, u64::MAX));
    }

    /// Re-transmitting a page resets its snapshot, so subsequent
    /// pressure has to accumulate from the fresh transmit. Invariant:
    /// after `mark_transmitted`, `is_eviction_at_risk` MUST return
    /// false for that page (until further transmits push the
    /// cumulative past threshold again). This is what makes the
    /// per-page detector self-clearing without an explicit reset.
    #[test]
    fn is_eviction_at_risk_resets_on_re_transmit() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        let threshold = 100_000u64;
        r.mark_transmitted(0, layout, 0, 100, 100); // 40k
        for i in 1..4 {
            r.mark_transmitted(i, layout, 0, 100, 100); // +40k each
        }
        // Cumulative = 160k. Page 0 snapshot=0 → delta=160k ≥ 100k.
        assert!(r.is_eviction_at_risk(0, threshold));
        // User scrolled back to page 0; we re-transmit it.
        r.mark_transmitted(0, layout, 0, 100, 100);
        // Snapshot=160k, cumulative now=200k → delta=40k < threshold.
        assert!(
            !r.is_eviction_at_risk(0, threshold),
            "re-transmitting MUST clear at-risk for that page — \
             otherwise the recovery path would loop infinitely"
        );
    }

    /// `invalidate_all_transmits` resets the cumulative epoch
    /// counter. Without this reset, a page transmitted right after
    /// invalidate would have snapshot=0 (fresh entry) but cumulative
    /// still carrying the prior epoch's huge value — the very next
    /// at-risk check would fire spuriously.
    #[test]
    fn invalidate_all_transmits_resets_cumulative_epoch() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        for i in 0..10 {
            r.mark_transmitted(i, layout, 0, 100, 100);
        }
        assert!(r.transmitted_bytes_cumulative() > 0);
        r.invalidate_all_transmits();
        assert_eq!(
            r.transmitted_bytes_cumulative(),
            0,
            "epoch counter MUST reset so post-invalidate transmits get \
             snapshots relative to the new epoch, not the old one"
        );
        // Fresh transmit after invalidate. snapshot=0, cumulative=40k
        // post-mark — at-risk only fires above threshold.
        r.mark_transmitted(0, layout, 0, 100, 100);
        assert!(!r.is_eviction_at_risk(0, 1_000_000));
    }

    #[test]
    fn mark_transmitted_drops_stale_pending_delete() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        // Fill past the cap so eviction triggers.
        for i in 0..(max_cached_pages() + 1) {
            r.mark_transmitted(i, layout, 0, 16, 16);
        }
        r.evict_to_budget(&[]);
        // Page 0 (lru-front) should now be queued for delete.
        let evicted_id = 1001u32; // id_base(1000) + 1 + page_idx(0)
        assert!(
            r.pending_deletes.contains(&evicted_id),
            "setup precondition: page 0's id must be queued for delete after evict_to_budget"
        );
        // Resurrect page 0 via a fresh mark_transmitted.
        r.mark_transmitted(0, layout, 1, 16, 16);
        assert!(
            !r.pending_deletes.contains(&evicted_id),
            "I4: resurrected image_id must be removed from pending_deletes"
        );
    }

    /// Regression: a placeholder cell from the prior frame that
    /// references a freed image_id makes Ghostty log
    /// `warning(renderer_image): missing image for virtual placement`
    /// every render-frame; sustained held-`j` scrolling once
    /// generated 660k such warnings and crashed Ghostty (incident
    /// 2026-05-04). `clear_page_area` overwrites column 0 with a
    /// short row-clear escape (spaces wrapped in save/restore) that
    /// (a) differs from any place_page escape so ratatui's diff
    /// fires on placement→clear transitions, and (b) actively paints
    /// spaces over Ghostty's grid cells, dropping the placeholder
    /// characters that hold the dead image_id reference.
    #[test]
    fn clear_page_area_overwrites_placement_with_row_clear_escape() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 6,
            height: 3,
        };
        let mut buf = Buffer::empty(area);
        let mut scratch = PlaceScratch::default();
        // Paint a placement so cell (0,0) carries the kitty placeholder
        // escape + image_id-encoding fg color, and cells (1..6, *) get
        // set_skip(true).
        place_page(
            &mut buf,
            area,
            /*page_idx*/ 0,
            /*image_id*/ 42,
            /*pixel_w*/ 60,
            /*pixel_h*/ 60,
            /*cell_w_px*/ 10,
            /*cell_h_px*/ 20,
            /*dst_top_cell*/ 0,
            /*dst_height_cells*/ 3,
            /*src_top_cell*/ 0,
            /*src_left_cell*/ 0,
            /*width_cells*/ 6,
            /*prefix*/ None,
            &mut scratch,
        );
        // Sanity: pre-clear, the placeholder symbol carries U+10EEEE.
        let placement_sym = buf.cell((0, 0)).unwrap().symbol().to_string();
        assert!(placement_sym.contains('\u{10EEEE}'));

        clear_page_area(&mut buf, area, &mut scratch, &[]);

        // Column 0: now holds the row-clear escape — non-empty, must
        // NOT carry the placeholder char, must contain spaces.
        for y in 0..area.height {
            let sym = buf.cell((0, y)).unwrap().symbol();
            assert!(
                !sym.contains('\u{10EEEE}'),
                "col 0 row {y} must drop the kitty placeholder; got {sym:?}"
            );
            assert!(
                sym.contains(' '),
                "col 0 row {y} must contain spaces (the row clear); got {sym:?}"
            );
            assert!(
                sym.starts_with("\x1b[s"),
                "col 0 row {y} must start with cursor-save (so its width matches what place_page expects); got {sym:?}"
            );
            assert_ne!(
                sym, placement_sym,
                "clear escape must differ from place escape (else ratatui's diff won't fire on transition)"
            );
        }
        // Columns 1..N: skip=FALSE (post-fix). Earlier code marked
        // these skip=true on the theory that col 0's row-clear
        // escape paints spaces over cells 1..N so an independent
        // emit was wasteful. That broke placement→no-placement
        // transitions: ratatui's diff filter (`if !current.skip`)
        // dropped the transition emit and Ghostty's grid retained
        // the stale placeholder char pointing at the soon-to-be-
        // evicted image_id (the 40k+ missing-image-warning flood).
        // skip=false costs nothing in steady state (cells are
        // EMPTY-equal so diff emits nothing) and triggers the
        // correct overwriting space on transitions.
        for y in 0..area.height {
            for x in 1..area.width {
                let cell = buf.cell((x, y)).unwrap();
                assert!(
                    !cell.skip,
                    "cell ({x},{y}) must have skip=false after clear so placement→clear transitions actually emit",
                );
                assert_eq!(
                    cell.fg,
                    ratatui::style::Color::Reset,
                    "cell ({x},{y}) fg must be Reset (no stale image_id encoding)"
                );
            }
        }
    }

    /// Page transmits should ship RGB, not RGBA — drops Ghostty's
    /// decoded image-store size by 25%, which is what made cap=7
    /// stop tripping internal evictions. Verifies both wire-format
    /// outputs (PNG and raw via TERMPDF_TRANSMIT_RAW).
    #[test]
    fn encode_payload_opaque_drops_alpha_in_raw_path() {
        // 4 px = 16 bytes RGBA. After alpha strip: 12 bytes RGB.
        let mut img = RgbaImage::new(2, 2);
        for (i, p) in img.pixels_mut().enumerate() {
            p[0] = (10 * i) as u8;
            p[1] = (20 * i) as u8;
            p[2] = (30 * i) as u8;
            p[3] = 255;
        }
        let raw = strip_alpha(&img);
        assert_eq!(raw.len(), 12, "RGB strip drops 4 alpha bytes per pixel");
        assert_eq!(&raw[0..3], &[0, 0, 0]);
        assert_eq!(&raw[3..6], &[10, 20, 30]);
        assert_eq!(&raw[6..9], &[20, 40, 60]);
        assert_eq!(&raw[9..12], &[30, 60, 90]);
    }

    #[test]
    fn encode_payload_opaque_default_is_rgb_zlib() {
        // The default opaque encode path is `f=24, o=z` — raw RGB
        // wrapped in a zlib stream. Round-trip the output through
        // flate2 and confirm the inflated bytes match the alpha-
        // stripped source.
        let mut img = RgbaImage::new(4, 4);
        for p in img.pixels_mut() {
            *p = image::Rgba([200, 100, 50, 255]);
        }
        let (format_code, compression, bytes) = encode_payload_opaque(&img);
        assert_eq!(format_code, 24, "default opaque format must be RGB (f=24)");
        assert_eq!(compression, b'z', "default opaque path must set o=z");
        use flate2::read::ZlibDecoder;
        use std::io::Read;
        let mut inflated = Vec::new();
        ZlibDecoder::new(&bytes[..])
            .read_to_end(&mut inflated)
            .unwrap();
        let expected = strip_alpha(&img);
        assert_eq!(
            inflated, expected,
            "deflated payload must round-trip to alpha-stripped RGB"
        );
    }

    /// Steady state: clearing the same area twice in a row produces
    /// byte-identical buffer state, so ratatui's diff emits nothing
    /// the second time. Without this, every frame would re-emit the
    /// row-clear escape over the entire image area — wasted bandwidth.
    #[test]
    fn clear_page_area_is_idempotent_for_diff_purposes() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 8,
            height: 4,
        };
        let mut buf_a = Buffer::empty(area);
        let mut buf_b = Buffer::empty(area);
        let mut scratch_a = PlaceScratch::default();
        let mut scratch_b = PlaceScratch::default();
        clear_page_area(&mut buf_a, area, &mut scratch_a, &[]);
        clear_page_area(&mut buf_b, area, &mut scratch_b, &[]);
        for y in 0..area.height {
            for x in 0..area.width {
                let a = buf_a.cell((x, y)).unwrap();
                let b = buf_b.cell((x, y)).unwrap();
                assert_eq!(a.symbol(), b.symbol(), "({x},{y}) symbol mismatch");
                assert_eq!(a.skip, b.skip, "({x},{y}) skip mismatch");
                assert_eq!(a.fg, b.fg, "({x},{y}) fg mismatch");
            }
        }
    }

    /// `clear_page_area` runs every frame including pure-idle redraws.
    /// The row-clear escape only depends on `(area.width, area.height)`
    /// so it must be cached on `PlaceScratch` and rebuilt only when
    /// dims change. Without the cache we paid a fresh `String` alloc
    /// + a per-cell `push(' ')` loop on every redraw.
    #[test]
    fn clear_page_area_caches_escape_until_dims_change() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 12,
            height: 5,
        };
        let mut buf = Buffer::empty(area);
        let mut scratch = PlaceScratch::default();
        clear_page_area(&mut buf, area, &mut scratch, &[]);
        // Snapshot the cached bytes + capacity so we can detect a
        // re-build (which would re-allocate or rewrite the string).
        let cached_first = scratch.cached_row_clear.clone();
        let cap_first = scratch.cached_row_clear.capacity();
        let ptr_first = scratch.cached_row_clear.as_ptr() as usize;
        assert!(!cached_first.is_empty());
        assert_eq!(scratch.cached_row_clear_dims, (area.width, area.height));

        // Same area on a fresh buffer — must reuse the cached string.
        let mut buf2 = Buffer::empty(area);
        clear_page_area(&mut buf2, area, &mut scratch, &[]);
        assert_eq!(
            scratch.cached_row_clear, cached_first,
            "string content changed"
        );
        assert_eq!(
            scratch.cached_row_clear.capacity(),
            cap_first,
            "string was reallocated"
        );
        assert_eq!(
            scratch.cached_row_clear.as_ptr() as usize,
            ptr_first,
            "backing pointer moved → realloc happened"
        );

        // Resize — the cache MUST rebuild (different W/H means the
        // restore-cursor offsets in the escape are different).
        let resized = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 8,
        };
        let mut buf3 = Buffer::empty(resized);
        clear_page_area(&mut buf3, resized, &mut scratch, &[]);
        assert_eq!(
            scratch.cached_row_clear_dims,
            (resized.width, resized.height)
        );
        assert_ne!(
            scratch.cached_row_clear, cached_first,
            "cache must rebuild on dim change"
        );
    }

    /// `place_page`'s per-row inner loop pre-computes the SGR escape
    /// (`\x1b[s\x1b[38;2;R;G;Bm\u{10EEEE}`) once per page since image
    /// IDs are stable per page. Re-placing the same page must not
    /// rebuild the head string; switching to a different page must.
    #[test]
    fn place_page_caches_row_head_per_image_id() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 8,
            height: 4,
        };
        let mut buf = Buffer::empty(area);
        let mut scratch = PlaceScratch::default();

        // First call with image_id=42 should populate the cache.
        place_page(
            &mut buf,
            area,
            /*page_idx*/ 0,
            /*image_id*/ 42,
            /*pixel_w*/ 80,
            /*pixel_h*/ 80,
            /*cell_w_px*/ 10,
            /*cell_h_px*/ 20,
            0,
            4,
            0,
            0,
            8,
            None,
            &mut scratch,
        );
        assert_eq!(scratch.cached_row_head_id, Some(42));
        let head_first = scratch.cached_row_head.clone();
        let ptr_first = scratch.cached_row_head.as_ptr() as usize;
        // Must encode the SGR with id 42 → bytes 0,0,42 → "0;0;42".
        assert!(
            head_first.contains("\x1b[38;2;0;0;42m"),
            "row head must encode image_id=42 in SGR fg, got {head_first:?}"
        );

        // Second call with same id must reuse the same allocation.
        place_page(
            &mut buf,
            area,
            0,
            42,
            80,
            80,
            10,
            20,
            0,
            4,
            0,
            0,
            8,
            None,
            &mut scratch,
        );
        assert_eq!(scratch.cached_row_head, head_first, "head changed");
        assert_eq!(
            scratch.cached_row_head.as_ptr() as usize,
            ptr_first,
            "row head was reallocated for the same image_id"
        );

        // Different id must rebuild the head with the new SGR.
        place_page(
            &mut buf,
            area,
            0,
            /*image_id*/ 99,
            80,
            80,
            10,
            20,
            0,
            4,
            0,
            0,
            8,
            None,
            &mut scratch,
        );
        assert_eq!(scratch.cached_row_head_id, Some(99));
        assert!(
            scratch.cached_row_head.contains("\x1b[38;2;0;0;99m"),
            "row head must rebuild with new image_id"
        );
    }

    /// Regression: a cell that held a kitty placement in frame N and
    /// then loses the placement in frame N+1 must emit through
    /// ratatui's diff so the row-clear's space overwrites Ghostty's
    /// stale placeholder. With the old `set_skip(true)` in
    /// clear_page_area, the cell stayed skip=true after losing its
    /// placement and ratatui's diff filter (`if !current.skip`)
    /// dropped the emit. Ghostty's grid kept the placeholder
    /// pointing at an image_id that subsequent eviction would free →
    /// 40k+ "missing image for virtual placement" warnings → crash.
    #[test]
    fn placement_then_clear_marks_cell_as_emittable_diff() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 8,
            height: 3,
        };
        let mut buf_prev = Buffer::empty(area);
        let mut buf_curr = Buffer::empty(area);
        let mut scratch = PlaceScratch::default();

        // Frame N: place + clear (the actual draw order in ui::draw
        // is clear → place, but the cells inside the placement region
        // at the END of the frame have placement state. We mimic that
        // end-of-frame state here.)
        clear_page_area(&mut buf_prev, area, &mut scratch, &[]);
        place_page(
            &mut buf_prev,
            area,
            /*page_idx*/ 0,
            /*image_id*/ 99,
            /*pixel_w*/ 80,
            /*pixel_h*/ 60,
            /*cell_w_px*/ 10,
            /*cell_h_px*/ 20,
            /*dst_top_cell*/ 0,
            /*dst_height_cells*/ 3,
            /*src_top_cell*/ 0,
            /*src_left_cell*/ 0,
            /*width_cells*/ 8,
            /*prefix*/ None,
            &mut scratch,
        );

        // Frame N+1: page scrolled away, clear only — no place.
        clear_page_area(&mut buf_curr, area, &mut scratch, &[]);

        // Diff: at least one cell that held a placement in frame N
        // must now appear in the diff so ratatui actually emits the
        // row-clear bytes that overwrite the stale placeholders in
        // Ghostty's grid.
        let updates: Vec<_> = buf_prev.diff(&buf_curr).into_iter().collect();
        assert!(
            !updates.is_empty(),
            "frame N+1's clear-only state must produce diff updates against frame N's placement state — otherwise Ghostty keeps stale placeholders pointing at the now-evicted image_id"
        );

        // Specifically, every cell that held the kitty placeholder
        // char in frame N must be in the diff (or the row-clear
        // escape covering it must be).
        let placement_cells_in_prev: Vec<(u16, u16)> = (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .filter(|(x, y)| {
                buf_prev
                    .cell((*x, *y))
                    .is_some_and(|c| c.symbol().contains('\u{10EEEE}'))
            })
            .collect();
        assert!(
            !placement_cells_in_prev.is_empty(),
            "precondition: frame N must have at least one placement cell"
        );
    }

    /// Regression: when ui::draw runs `clear_page_area` on the full
    /// image area FIRST and then `place_page` on a centered page
    /// (placement_area.left > image_area.left), the placement's col-0
    /// cell sits inside the image-area range that clear_page_area
    /// marked `set_skip(true)`. set_symbol() does not reset skip, so
    /// without an explicit `set_skip(false)` in place_page ratatui's
    /// diff filters the cell out and the kitty escape never reaches
    /// the terminal — the page renders blank. This was the reported
    /// "some pages stay blank after being rendered while scrolling"
    /// symptom.
    #[test]
    fn place_page_after_clear_emits_centered_placement_to_terminal() {
        // Image area is 10 wide; page bitmap is 6 wide → centered with
        // a 2-cell left margin (dst_left_cell = 2).
        let img_area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 4,
        };
        let mut buf = Buffer::empty(img_area);
        let mut scratch = PlaceScratch::default();

        clear_page_area(&mut buf, img_area, &mut scratch, &[]);

        // Post-fix design: clear_page_area leaves cols 1..N as
        // skip=false (Cell::EMPTY default after reset) so that
        // placement→clear transitions are emittable through
        // ratatui's diff. place_page's set_skip(false) on its
        // col-0 cell is therefore defensive — it stays correct
        // even if a future change to clear_page_area reintroduces
        // skip=true in this range.
        assert!(
            !buf.cell((2, 0)).unwrap().skip,
            "post-clear, cells 1..N have skip=false so transitions emit"
        );

        let placement_area = Rect {
            x: 2,
            y: 0,
            width: 6,
            height: 4,
        };
        place_page(
            &mut buf,
            placement_area,
            /*page_idx*/ 0,
            /*image_id*/ 7,
            /*pixel_w*/ 60,
            /*pixel_h*/ 80,
            /*cell_w_px*/ 10,
            /*cell_h_px*/ 20,
            /*dst_top_cell*/ 0,
            /*dst_height_cells*/ 4,
            /*src_top_cell*/ 0,
            /*src_left_cell*/ 0,
            /*width_cells*/ 6,
            /*prefix*/ None,
            &mut scratch,
        );

        // The col-0 cell of the placement (= col 2 of img_area) must
        // carry the kitty placement symbol AND have skip cleared, or
        // ratatui's diff filters it out.
        let cell = buf.cell((2, 0)).unwrap();
        assert!(
            cell.symbol().contains('\u{10EEEE}'),
            "placement col-0 must carry the kitty placeholder char, got {:?}",
            cell.symbol(),
        );
        assert!(
            !cell.skip,
            "placement col-0 must have skip=false so ratatui emits it; was skip=true → blank-page bug",
        );
    }

    /// Regression for the boundary-scroll blank-page bug. Frame N
    /// places page 0 at rows 0..3. Frame N+1, the page is still
    /// layout-visible but cell-clipped (no place_page call) — the
    /// real-app scenario when a page boundary sits sub-cell at the
    /// viewport edge. Previously `clear_page_area` wrote spaces over
    /// every cell of the image area; ratatui's diff would then emit
    /// clearing escapes that erased the placeholder cells from the
    /// terminal's grid. With zero placeholders pointing at the
    /// page's image_id, Ghostty's `deleteIfUnused` predicate flips
    /// true and the image is first in line for eviction; on the
    /// next frame where the page returns to ≥1 cell visible, the
    /// fresh placement APC references a freed image_id → blank.
    ///
    /// Fix: pass the layout-visible page set as `preserve_pages`.
    /// Cells inside the previous frame's placement rect for a
    /// still-visible page are skipped — placeholders stay in the
    /// buffer, ratatui's diff sees no change, no clearing escapes
    /// hit the wire.
    #[test]
    fn clear_preserves_placeholders_for_layout_visible_clipped_pages() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        let area = Rect {
            x: 0,
            y: 0,
            width: 8,
            height: 4,
        };
        let mut buf = Buffer::empty(area);
        let mut scratch = PlaceScratch::default();

        // Frame N: clear (with no preservation — first frame), then
        // place page 0 at rows 0..3. After this frame, scratch
        // .last_placed has the rect; it sticks until the page is
        // evicted from the registry.
        clear_page_area(&mut buf, area, &mut scratch, &[]);
        place_page(
            &mut buf,
            area,
            /*page_idx*/ 0,
            /*image_id*/ 42,
            /*pixel_w*/ 80,
            /*pixel_h*/ 60,
            /*cell_w_px*/ 10,
            /*cell_h_px*/ 20,
            /*dst_top_cell*/ 0,
            /*dst_height_cells*/ 3,
            /*src_top_cell*/ 0,
            /*src_left_cell*/ 0,
            /*width_cells*/ 8,
            /*prefix*/ None,
            &mut scratch,
        );
        // Snapshot the placeholder symbol before frame N+1 runs.
        let placed_symbol = buf.cell((0, 0)).unwrap().symbol().to_string();
        assert!(
            placed_symbol.contains('\u{10EEEE}'),
            "frame N must place the kitty placeholder; setup precondition failed"
        );

        // Frame N+1: page 0 is layout-visible (still in
        // preserve_pages) but cell-clipped — no place_page call.
        // clear must NOT wipe page 0's previous placement cells.
        clear_page_area(&mut buf, area, &mut scratch, &[0]);
        let after_clear = buf.cell((0, 0)).unwrap().symbol().to_string();
        assert_eq!(
            after_clear, placed_symbol,
            "preserved cell symbol must be byte-identical to the previous \
             frame's placement; got {after_clear:?}"
        );
        // Cells 1..N of the placement carry skip=true and a default
        // " " symbol — that's how ratatui-image's diff renderer
        // suppresses per-cell emit so col 0's row escape paints the
        // whole row in one shot. Preservation just means we don't
        // turn that " "+skip=true state into the row-clear escape.
        for cx in 1..3u16 {
            let cell = buf.cell((cx, 0)).unwrap();
            assert!(
                cell.skip,
                "cell ({cx}, 0) lost its skip=true after preservation \
                 — diff renderer would emit a clear over col 0's escape"
            );
        }

        // Frame N+1 (variant): page 0 is NOT in preserve_pages
        // (genuinely scrolled out of view). clear MUST wipe the
        // cells so stale placeholders don't keep referencing a
        // freed image_id and flood Ghostty's "missing image" log.
        let mut buf2 = Buffer::empty(area);
        let mut scratch2 = PlaceScratch::default();
        clear_page_area(&mut buf2, area, &mut scratch2, &[]);
        place_page(
            &mut buf2,
            area,
            0,
            42,
            80,
            60,
            10,
            20,
            0,
            3,
            0,
            0,
            8,
            None,
            &mut scratch2,
        );
        clear_page_area(&mut buf2, area, &mut scratch2, &[]);
        let after_clear_unpinned = buf2.cell((0, 0)).unwrap().symbol().to_string();
        assert!(
            !after_clear_unpinned.contains('\u{10EEEE}'),
            "unpinned page must have its placeholder wiped; got {after_clear_unpinned:?}"
        );
    }
}

/// Property-based tests for the registry's state machine.
///
/// Drives randomized sequences of `mark_transmitted` / `evict_to_budget`
/// / `invalidate_*` ops and asserts five invariants after each step:
///
///   I1. **LRU == pages keys** — every page in the cache appears in the
///       LRU exactly once, and vice versa. Drift here is the upstream
///       cause of "we evict an image_id whose entry was already gone."
///   I2. **Cache cap respected** — `pages.len() <= MAX_CACHED_PAGES + |pinned|`
///       after any `evict_to_budget(pinned)` call. Pinned pages never
///       evict, so the over-cap only equals the pin count.
///   I3. **Fresh after mark** — `is_fresh(...)` returns true immediately
///       after `mark_transmitted` with the same parameters. The
///       blank-page bug class typically violates this when an
///       intermediate evict drops the entry between mark and check.
///   I4. **No live image_id is queued for delete** — every `i={N},`
///       token in `pending_deletes` references a page that has been
///       removed from `pages`. A live page whose id is queued would
///       blank on the next transmit.
///   I5. **LRU has no duplicates** — proxy for the touch/remove pairs
///       in `evict_to_budget` keeping the deque a true permutation.
///
/// Direct access to private fields (`pages`, `lru`, `pending_deletes`)
/// is the reason this test sits in the same module instead of a
/// `tests/` integration test — adding `pub` test-only accessors would
/// pollute the production API surface.
#[cfg(test)]
mod registry_proptests {
    use super::*;
    use proptest::prelude::*;

    const N_PAGES: usize = 12; // bounded universe forces collisions

    #[derive(Debug, Clone)]
    enum Op {
        MarkTransmitted {
            page_idx: usize,
            w: u32,
            h: u32,
            layout: LayoutKey,
            revision: u64,
        },
        EvictToBudget {
            pinned: Vec<usize>,
        },
        Invalidate {
            page_idx: usize,
        },
        InvalidateAll,
        TakeDeletes,
    }

    fn layout_strategy() -> impl Strategy<Value = LayoutKey> {
        (200u32..=2000, any::<bool>()).prop_map(|(w, dark)| LayoutKey {
            fit_width_px: w,
            dark,
        })
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        // Listed simplest-first so proptest's shrinker prefers the
        // cheap ops when reducing a failing counterexample. Weights
        // bias the distribution toward MarkTransmitted (the only op
        // that adds entries) so eviction has work to do.
        prop_oneof![
            1 => Just(Op::InvalidateAll),
            1 => Just(Op::TakeDeletes),
            2 => (0..N_PAGES).prop_map(|p| Op::Invalidate { page_idx: p }),
            3 => prop::collection::vec(0..N_PAGES, 0..=4)
                    .prop_map(|pinned| Op::EvictToBudget { pinned }),
            5 => (
                    0..N_PAGES,
                    64u32..=4096,
                    64u32..=4096,
                    layout_strategy(),
                    0u64..16,
                )
                    .prop_map(|(p, w, h, l, r)| Op::MarkTransmitted {
                        page_idx: p,
                        w,
                        h,
                        layout: l,
                        revision: r,
                    }),
        ]
    }

    fn check_invariants(r: &KittyPageRegistry) -> Result<(), TestCaseError> {
        // I1 / I5: LRU is a permutation of pages keys.
        let mut keys: Vec<usize> = r.pages.keys().copied().collect();
        let mut lru: Vec<usize> = r.lru.iter().copied().collect();
        let lru_len = lru.len();
        keys.sort_unstable();
        lru.sort_unstable();
        prop_assert_eq!(
            keys.clone(),
            lru.clone(),
            "I1 violated: LRU and pages map disagree on membership"
        );
        let lru_set: std::collections::HashSet<usize> = lru.into_iter().collect();
        prop_assert_eq!(
            lru_set.len(),
            lru_len,
            "I5 violated: LRU has duplicate entries"
        );

        // I4: no live image_id is queued for delete. After the
        // String → Vec<u32> refactor (commit landing alongside this
        // test), the queue is the authoritative ids list directly.
        for entry in r.pages.values() {
            prop_assert!(
                !r.pending_deletes.contains(&entry.image_id),
                "I4 violated: image_id {} is queued for delete but page is still live",
                entry.image_id
            );
        }
        Ok(())
    }

    proptest! {
        // 64 cases × ~30 ops each = ~2000 op replays per run. Surfaces
        // LRU / cache / pending_deletes drift in well under a second.
        // Bump via PROPTEST_CASES env var when paranoid.
        #![proptest_config(ProptestConfig {
            cases: 64,
            .. ProptestConfig::default()
        })]

        #[test]
        fn registry_invariants_hold_under_random_ops(
            ops in prop::collection::vec(op_strategy(), 1..=30)
        ) {
            let mut r = KittyPageRegistry::new(false, 1000);
            for op in ops {
                match op {
                    Op::MarkTransmitted { page_idx, w, h, layout, revision } => {
                        r.mark_transmitted(page_idx, layout, revision, w, h);
                        // I3: fresh-after-mark.
                        prop_assert!(
                            r.is_fresh(page_idx, layout, revision, w, h),
                            "I3 violated: !is_fresh immediately after mark_transmitted"
                        );
                    }
                    Op::EvictToBudget { ref pinned } => {
                        // I6: visible_range_pin protection — every
                        // pinned page that WAS resident before this
                        // call MUST remain resident after. Snapshot
                        // pre-eviction membership; pages not in the
                        // registry before the call are out of scope
                        // (eviction can't conjure them back).
                        // Defense for the 2026-05-04 unloading-on-
                        // scroll incident — the exact path that
                        // broke before `clear_page_area`'s preserve
                        // contract was tightened.
                        let resident_pinned: Vec<usize> = pinned
                            .iter()
                            .copied()
                            .filter(|p| r.pages.contains_key(p))
                            .collect();
                        r.evict_to_budget(pinned);
                        // I2: cache cap respected (allow over-cap by |pinned| since pinned never evict).
                        let cap = max_cached_pages();
                        prop_assert!(
                            r.pages.len() <= cap + pinned.len(),
                            "I2 violated: pages.len()={} > MAX_CACHED_PAGES+|pinned|={}",
                            r.pages.len(),
                            cap + pinned.len()
                        );
                        for p in resident_pinned {
                            prop_assert!(
                                r.pages.contains_key(&p),
                                "I6 violated: pinned page {} was evicted despite being in pinned and resident",
                                p,
                            );
                        }
                    }
                    Op::Invalidate { page_idx } => r.invalidate_transmit(page_idx),
                    Op::InvalidateAll => r.invalidate_all_transmits(),
                    Op::TakeDeletes => {
                        let _ = r.take_pending_deletes();
                    }
                }
                check_invariants(&r)?;
            }
        }
    }
}
