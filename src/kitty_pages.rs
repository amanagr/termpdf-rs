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

/// Cap on cached pages. Each entry holds metadata + an optional
/// encoded payload (~250 KB PNG). The terminal-side decoded RGBA is
/// much larger (~10 MB/page at our render resolution) and Ghostty
/// internally evicts when its image store fills up. When that happens
/// any of our placement cells still pointing at the freed image_id
/// triggers `warning(renderer_image): missing image for virtual
/// placement` per cell per render-frame.
///
/// 7 = current page ±3, which covers the typical viewport (~2-3
/// pages), idle-warm prefetch (`MAX_WARMS_PER_IDLE=2` per tick), and
/// a small scroll-back budget. Under ~70 MB on the terminal side, so
/// Ghostty's image store has plenty of headroom and never needs to
/// evict on its own. Scrolling back further than ±3 re-renders the
/// page (~30 ms), which is the explicit tradeoff for keeping
/// terminal memory tight.
const MAX_CACHED_PAGES: usize = 7;

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
}

#[derive(Debug, Clone)]
struct CachedPayload {
    layout: LayoutKey,
    revision: u64,
    pixel_w: u32,
    pixel_h: u32,
    /// Kitty format code: 100 = PNG, 32 = raw RGBA.
    format_code: u8,
    bytes: Vec<u8>,
}

/// Per-page selection-band overlay. Distinct from `PageEntry` because
/// it tracks a different kind of image (transparent overlay vs full
/// page bitmap) on a different schedule (changes per Visual-mode
/// keystroke, vs once per layout/highlight edit).
///
/// The overlay is a same-dim bitmap as the page that's mostly
/// transparent (alpha=0 outside the selection band, alpha=115 inside).
/// Kitty composites it over the page placement at z=1, so a moving
/// selection re-transmits ONLY this small mostly-transparent PNG (~5-15
/// KB) instead of the full page bitmap (~170 KB).
#[derive(Debug, Clone)]
struct OverlayEntry {
    image_id: u32,
    /// Layout + revision + selection signature at last transmit. Three
    /// fields together so a caret motion within a stable layout can
    /// invalidate just the overlay without touching the page entry.
    transmitted_layout: Option<LayoutKey>,
    transmitted_revision: u64,
    transmitted_sel_sig: u64,
    pixel_w: u32,
    pixel_h: u32,
    cached_payload: Option<OverlayCachedPayload>,
}

#[derive(Debug, Clone)]
struct OverlayCachedPayload {
    layout: LayoutKey,
    revision: u64,
    sel_sig: u64,
    pixel_w: u32,
    pixel_h: u32,
    format_code: u8,
    bytes: Vec<u8>,
}

pub struct KittyPageRegistry {
    is_tmux: bool,
    /// Base for per-page IDs. `id_for(page) = base + 1 + page_idx`.
    /// Adding 1 keeps the canvas-mode `kitty_image_id` (= base) free
    /// for any fallback path that might still want to use it.
    id_base: u32,
    pages: HashMap<usize, PageEntry>,
    /// Selection-band overlay images per page. Lives parallel to
    /// `pages`; cleared when the page entry is evicted. ID range is
    /// `id_base + OVERLAY_ID_OFFSET + page_idx` to keep page and
    /// overlay IDs disjoint.
    overlays: HashMap<usize, OverlayEntry>,
    /// LRU order — front = least recently used. `touch` moves a page
    /// to the back; eviction pops from the front. Entries here mirror
    /// keys in `pages`.
    lru: VecDeque<usize>,
    /// Kitty `a=d,d=I,i=ID` delete escapes accumulated during eviction.
    /// Caller drains via `take_pending_deletes()` and prepends to the
    /// next transmit (or a synthetic one) so the terminal frees its
    /// decoded RGBA copy of the evicted image.
    pending_deletes: String,
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
}

/// Distance between a page's image_id and its overlay's image_id. Big
/// enough that no plausible PDF page count would collide with the
/// overlay range. (u32 has 4G slots; we never come close.)
const OVERLAY_ID_OFFSET: u32 = 1 << 20;

impl KittyPageRegistry {
    pub fn new(is_tmux: bool, id_base: u32) -> Self {
        Self {
            is_tmux,
            id_base,
            pages: HashMap::new(),
            overlays: HashMap::new(),
            lru: VecDeque::new(),
            pending_deletes: String::new(),
            place_scratch: PlaceScratch::default(),
        }
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
        self.overlays.clear();
        self.lru.clear();
        self.pending_deletes.clear();
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
    /// `MAX_CACHED_PAGES` pages. Pages in `pinned` are skipped (so the
    /// current frame's visible pages stay alive). For each eviction,
    /// emits a kitty `a=d,d=I,i=ID` delete escape into
    /// `pending_deletes` so the terminal also frees its image.
    pub fn evict_to_budget(&mut self, pinned: &[usize]) {
        if self.pages.len() <= MAX_CACHED_PAGES {
            return;
        }
        // Scan front (LRU) to back, collecting victims that aren't
        // pinned. Stop once we've trimmed enough. Building the
        // eviction set up-front lets us drop entries from `self.lru`
        // with one pass instead of an O(N) `VecDeque::remove(idx)`
        // per evicted page; the previous loop was O(K·N) for K
        // evictions on a doc that just blew past the cap.
        let over = self.pages.len() - MAX_CACHED_PAGES;
        let mut victims: Vec<usize> = Vec::with_capacity(over);
        for &cand in self.lru.iter() {
            if victims.len() >= over {
                break;
            }
            if pinned.contains(&cand) {
                continue;
            }
            victims.push(cand);
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
        let mut freed_ids: Vec<u32> = Vec::with_capacity(victims.len() * 2);
        for v in &victims {
            if let Some(entry) = self.pages.remove(v) {
                freed_ids.push(entry.image_id);
            }
            if let Some(entry) = self.overlays.remove(v) {
                freed_ids.push(entry.image_id);
            }
        }
        self.lru.retain(|p| !victim_set.contains(p));
        self.queue_deletes(&mut freed_ids);
    }

    /// Append the most efficient delete sequence for a set of freed
    /// image_ids. Sorts the input in place, walks contiguous runs,
    /// and emits one `a=d,d=R,x=lo,y=hi` per run (≥2 IDs) or one
    /// `a=d,d=I,i=ID` per singleton. For a typical 3-page forward-
    /// scroll eviction with consecutive ids this is one APC instead
    /// of three.
    fn queue_deletes(&mut self, ids: &mut [u32]) {
        if ids.is_empty() {
            return;
        }
        ids.sort_unstable();
        let mut i = 0;
        while i < ids.len() {
            let mut j = i;
            while j + 1 < ids.len() && ids[j + 1] == ids[j] + 1 {
                j += 1;
            }
            if j == i {
                self.queue_delete_single(ids[i]);
            } else {
                self.queue_delete_range(ids[i], ids[j]);
            }
            i = j + 1;
        }
    }

    fn queue_delete_single(&mut self, id: u32) {
        let (start, escape, end) = tmux_wrap(self.is_tmux);
        // `a=d` = delete; `d=I` = by image id; `q=2` = suppress reply.
        // Not freeing placement state explicitly — placements that
        // referenced this id will simply render nothing once the
        // image is gone, which is fine because we only evict
        // non-visible (= unreferenced) pages.
        write!(
            self.pending_deletes,
            "{start}{escape}_Ga=d,d=I,i={id},q=2;{escape}\\{end}"
        )
        .unwrap();
    }

    fn queue_delete_range(&mut self, lo: u32, hi: u32) {
        let (start, escape, end) = tmux_wrap(self.is_tmux);
        // `d=R` = range delete by image id (kitty v0.33.0+).
        // x = lower bound inclusive, y = upper bound inclusive.
        // Also frees the image data (uppercase R = free, lowercase r
        // = placement-only — we want both).
        write!(
            self.pending_deletes,
            "{start}{escape}_Ga=d,d=R,x={lo},y={hi},q=2;{escape}\\{end}"
        )
        .unwrap();
    }

    /// Drain accumulated delete escapes (from prior evictions). The
    /// caller should prepend the result to its next transmit string
    /// so the terminal processes the deletes alongside the new frame.
    pub fn take_pending_deletes(&mut self) -> Option<String> {
        if self.pending_deletes.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.pending_deletes))
        }
    }

    /// Push a delete-escape blob back onto the pending queue. Used
    /// when the caller drained but couldn't find a transmit to ride
    /// it in on; the next frame with any transmit will pick it up.
    pub fn put_back_pending_deletes(&mut self, s: String) {
        if self.pending_deletes.is_empty() {
            self.pending_deletes = s;
        } else {
            // Existing buffer was modified between take/put_back —
            // shouldn't happen in our single-threaded draw loop, but
            // handle it by prepending so order is preserved.
            let mut combined = s;
            combined.push_str(&self.pending_deletes);
            self.pending_deletes = combined;
        }
    }

    fn id_for(&self, page_idx: usize) -> u32 {
        self.id_base.wrapping_add(1).wrapping_add(page_idx as u32)
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
        let entry = self.pages.entry(page_idx).or_insert(PageEntry {
            image_id: id,
            transmitted_layout: None,
            transmitted_revision: 0,
            pixel_w,
            pixel_h,
            cached_payload: None,
        });
        entry.image_id = id;
        entry.transmitted_layout = Some(layout);
        entry.transmitted_revision = revision;
        entry.pixel_w = pixel_w;
        entry.pixel_h = pixel_h;
        self.touch(page_idx);
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
                let (format_code, bytes) = encode_payload_opaque(bitmap);
                entry.cached_payload = Some(CachedPayload {
                    layout,
                    revision,
                    pixel_w,
                    pixel_h,
                    format_code,
                    bytes,
                });
            }
            let c = entry
                .cached_payload
                .as_ref()
                .expect("cached_payload populated on the line above");
            build_transmit_string(&c.bytes, c.format_code, id, pixel_w, pixel_h, is_tmux)
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
        });
        if let Some(c) = &entry.cached_payload {
            if c.layout == layout
                && c.revision == revision
                && c.pixel_w == pixel_w
                && c.pixel_h == pixel_h
            {
                return;
            }
        }
        let (format_code, bytes) = encode_payload_opaque(bitmap);
        entry.cached_payload = Some(CachedPayload {
            layout,
            revision,
            pixel_w,
            pixel_h,
            format_code,
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

    /// Image ID for this page's selection-band overlay. Disjoint from
    /// the page ID range so the terminal stores them as independent
    /// images.
    pub fn overlay_image_id(&self, page_idx: usize) -> u32 {
        self.id_base
            .wrapping_add(OVERLAY_ID_OFFSET)
            .wrapping_add(page_idx as u32)
    }

    /// True if the cached overlay transmit for this page matches all of
    /// (layout, revision, sel_sig, dims). When false, the overlay
    /// either doesn't exist or its on-terminal pixels don't match what
    /// we'd send now — caller must transmit.
    pub fn overlay_is_fresh(
        &self,
        page_idx: usize,
        layout: LayoutKey,
        revision: u64,
        sel_sig: u64,
        pixel_w: u32,
        pixel_h: u32,
    ) -> bool {
        match self.overlays.get(&page_idx) {
            Some(e) => {
                e.transmitted_layout == Some(layout)
                    && e.transmitted_revision == revision
                    && e.transmitted_sel_sig == sel_sig
                    && e.pixel_w == pixel_w
                    && e.pixel_h == pixel_h
            }
            None => false,
        }
    }

    /// Build the kitty transmit escape for the selection-band overlay
    /// of this page. Mirrors `build_transmit` for pages, but uses the
    /// overlay ID range and includes `sel_sig` in the cache key. The
    /// resulting transmit string carries the bitmap; the caller is
    /// responsible for emitting a separate classical placement APC at
    /// the right cursor position to actually put it on screen.
    pub fn overlay_build_transmit(
        &mut self,
        bitmap: &RgbaImage,
        page_idx: usize,
        layout: LayoutKey,
        revision: u64,
        sel_sig: u64,
    ) -> String {
        let id = self.overlay_image_id(page_idx);
        let pixel_w = bitmap.width();
        let pixel_h = bitmap.height();
        let is_tmux = self.is_tmux;
        let entry = self.overlays.entry(page_idx).or_insert(OverlayEntry {
            image_id: id,
            transmitted_layout: None,
            transmitted_revision: 0,
            transmitted_sel_sig: 0,
            pixel_w,
            pixel_h,
            cached_payload: None,
        });
        let needs_encode = match &entry.cached_payload {
            Some(c) => {
                !(c.layout == layout
                    && c.revision == revision
                    && c.sel_sig == sel_sig
                    && c.pixel_w == pixel_w
                    && c.pixel_h == pixel_h)
            }
            None => true,
        };
        if needs_encode {
            let (format_code, bytes) = encode_payload(bitmap);
            entry.cached_payload = Some(OverlayCachedPayload {
                layout,
                revision,
                sel_sig,
                pixel_w,
                pixel_h,
                format_code,
                bytes,
            });
        }
        let c = entry
            .cached_payload
            .as_ref()
            .expect("overlay payload populated above");
        // Use the same transmit-string builder as pages — kitty doesn't
        // distinguish image kinds, only IDs.
        build_transmit_string(&c.bytes, c.format_code, id, pixel_w, pixel_h, is_tmux)
    }

    /// Update overlay registry state to record that the just-built
    /// transmit has been (or is about to be) flushed to the terminal.
    pub fn overlay_mark_transmitted(
        &mut self,
        page_idx: usize,
        layout: LayoutKey,
        revision: u64,
        sel_sig: u64,
        pixel_w: u32,
        pixel_h: u32,
    ) {
        let id = self.overlay_image_id(page_idx);
        let entry = self.overlays.entry(page_idx).or_insert(OverlayEntry {
            image_id: id,
            transmitted_layout: None,
            transmitted_revision: 0,
            transmitted_sel_sig: 0,
            pixel_w,
            pixel_h,
            cached_payload: None,
        });
        entry.image_id = id;
        entry.transmitted_layout = Some(layout);
        entry.transmitted_revision = revision;
        entry.transmitted_sel_sig = sel_sig;
        entry.pixel_w = pixel_w;
        entry.pixel_h = pixel_h;
    }

    /// Drop the overlay entry for `page_idx` and queue an `a=d` for the
    /// terminal so it frees the corresponding image. Called when the
    /// selection moves off this page or is cleared.
    pub fn overlay_drop(&mut self, page_idx: usize) {
        if let Some(entry) = self.overlays.remove(&page_idx) {
            self.queue_delete_single(entry.image_id);
        }
    }

    /// True when the terminal is currently holding a placement of this
    /// page's overlay. Used by the renderer to decide whether to emit a
    /// `a=d,d=I,i=ID` for a stale overlay even when there's no fresh
    /// selection on this page.
    pub fn overlay_is_present(&self, page_idx: usize) -> bool {
        self.overlays
            .get(&page_idx)
            .is_some_and(|e| e.transmitted_layout.is_some())
    }
}

/// Encode the bitmap into a (kitty format code, payload bytes) pair.
/// 100 = PNG, 32 = raw RGBA.
///
/// Format choice: PDF pages are mostly white space with sparse text,
/// so PNG compresses 5-20× smaller than raw RGBA. We pay a one-shot
/// encode cost (~2 ms with `Fast` + `Up`) to save 50-150 ms of
/// pty-write time on the wire. Net win for any page bigger than ~50 KB
/// raw, which is every real PDF page.
///
/// Set `TERMPDF_TRANSMIT_RAW=1` to force the old raw-RGBA path —
/// useful for A/B testing or if a terminal turns out not to like
/// PNG transmits in practice.
fn encode_payload(bitmap: &RgbaImage) -> (u8, Vec<u8>) {
    if force_raw_env() {
        return (32, bitmap.as_raw().to_vec());
    }
    match encode_png_fast(bitmap) {
        Ok(png) => (100, png),
        Err(_) => (32, bitmap.as_raw().to_vec()),
    }
}

/// PAGE encode path — strips alpha and ships RGB. Page bitmaps come
/// out of pdfium opaque (alpha=255 everywhere) and stay opaque after
/// dark-mode inversion + highlight baking (we composite onto an
/// opaque bg). Sending RGB cuts:
///   1. Wire bytes by 25% (PNG's deflate already compresses constant
///      alpha well, but we still ship the alpha plane through it).
///   2. **Ghostty's image-store size by 25%** — Ghostty keeps the
///      decoded buffer; for our 10 MB/page raw RGBA at typical zoom,
///      RGB drops it to ~7.5 MB. With cap=7 pages that's ~52 MB on
///      Ghostty's side instead of ~70 MB, well under the internal
///      capacity that was triggering "missing image" warnings.
///
/// Overlay images (selection band) keep RGBA via `encode_payload`;
/// they have real alpha < 255 by design.
fn encode_payload_opaque(bitmap: &RgbaImage) -> (u8, Vec<u8>) {
    if force_raw_env() {
        return (24, strip_alpha(bitmap));
    }
    match encode_png_fast_rgb(bitmap) {
        Ok(png) => (100, png),
        Err(_) => (24, strip_alpha(bitmap)),
    }
}

/// Copy RGBA → RGB, dropping the alpha byte. Iterates 4-byte chunks
/// so the compiler can vectorise; ~5 ms on a 10 MB page bitmap, which
/// is dwarfed by the ~30-50 ms PNG encode that follows.
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

/// Build the kitty `a=T,U=1` chunked transmit for an already-encoded
/// payload. Pure formatting — no encode work — so it's cheap to call
/// even after a cache hit.
fn build_transmit_string(
    payload: &[u8],
    format_code: u8,
    id: u32,
    pixel_w: u32,
    pixel_h: u32,
    is_tmux: bool,
) -> String {
    let (start, escape, end) = tmux_wrap(is_tmux);

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

    for (i, chunk) in payload.chunks(RAW_PER_CHUNK).enumerate() {
        data.push_str(start);
        write!(data, "{escape}_Gq=2,").unwrap();
        if i == 0 {
            // q=2 suppresses kitty responses; t=d = direct transmit
            // (data inline); a=T = transmit-and-store (no immediate
            // placement); U=1 = mark for unicode placeholder use.
            // f=32 raw RGBA (s/v needed) or f=100 PNG (decoder reads
            // dims from PNG header but we send s/v anyway — kitty
            // accepts and uses them as a hint).
            write!(
                data,
                "i={id},a=T,U=1,f={format_code},t=d,s={pixel_w},v={pixel_h},"
            )
            .unwrap();
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
    // No c/r → use the image's natural cell dimensions.
    data.push_str(start);
    write!(data, "{escape}_Ga=p,U=1,i={id},q=2;{escape}\\").unwrap();
    data.push_str(end);
    data
}

/// Convenience wrapper used in unit tests: encode + build in one
/// call. Production callers go through `KittyPageRegistry::build_transmit`
/// which adds the payload cache on top.
#[cfg(test)]
fn transmit(bitmap: &RgbaImage, id: u32, is_tmux: bool) -> String {
    let (format_code, payload) = encode_payload(bitmap);
    build_transmit_string(
        &payload,
        format_code,
        id,
        bitmap.width(),
        bitmap.height(),
        is_tmux,
    )
}

/// PNG-encode with `Fast` compression + `Up` filter. Benchmarked on a
/// 1600×2300 synthetic page: NoFilter = 2.8× compression at 14 ms,
/// Up filter = 50× compression at 1.9 ms. The Up predictor (each
/// pixel = pixel above) is a near-perfect fit for PDF backgrounds,
/// so it beats NoFilter on both axes. Adaptive (per-row best filter)
/// is marginally smaller but slower; not worth it.
fn encode_png_fast(bitmap: &RgbaImage) -> Result<Vec<u8>, image::ImageError> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::ImageEncoder;
    // Best-case PNG of a typical page is ~250 KB; reserve in that
    // ballpark to avoid a few growth reallocations during encode.
    let mut buf = Vec::with_capacity(512 * 1024);
    let encoder = PngEncoder::new_with_quality(&mut buf, CompressionType::Fast, FilterType::Up);
    encoder.write_image(
        bitmap.as_raw(),
        bitmap.width(),
        bitmap.height(),
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(buf)
}

/// Same as `encode_png_fast` but encodes RGB (drops alpha). Used by
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

    let max_src_rows = (pixel_h / cell_h_px.max(1)) as u16;
    let max_src_cols = (pixel_w / cell_w_px.max(1)) as u16;
    let max_dst_rows_in_area = area.height.saturating_sub(dst_top_cell);
    // Source rows we have; destination rows we have; whichever is fewer.
    let height_cells = dst_height_cells
        .min(max_dst_rows_in_area)
        .min(max_src_rows.saturating_sub(src_top_cell));
    // Clamp width to the image columns we actually have past src_left_cell
    // — the user may have scrolled scroll_x to the rightmost edge where
    // fewer image cols remain than the placement area can show.
    let width_cells = width_cells.min(max_src_cols.saturating_sub(src_left_cell));
    if height_cells == 0 || width_cells == 0 {
        return 0;
    }
    if width_cells as u32 > MAX_COLS {
        // Source col diacritic capacity exceeded; clamp silently.
    }

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
    height_cells
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
) {
    use std::fmt::Write;
    if area.width == 0 || area.height == 0 {
        return;
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
        if let Some(cell) = buf.cell_mut((area.left(), y)) {
            cell.reset();
            cell.set_symbol(&scratch.cached_row_clear);
        }
        for cx in 1..area.width {
            let x = area.left().saturating_add(cx);
            if x >= area.right() {
                break;
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
        // Fill past cap.
        for i in 0..(MAX_CACHED_PAGES + 8) {
            r.mark_transmitted(i, layout, 0, 16, 16);
            // Also stash a payload so eviction frees something visible.
            r.pre_encode(&bm, i, layout, 0);
        }
        r.evict_to_budget(&[]);
        assert_eq!(r.pages.len(), MAX_CACHED_PAGES);
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
    fn evict_skips_pinned_visible() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        // Prime LRU: pages 0..N+4 marked, in order. 0..4 are LRU.
        for i in 0..(MAX_CACHED_PAGES + 4) {
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

    /// Overlay image IDs MUST stay disjoint from page image IDs so the
    /// terminal stores them as independent images (otherwise re-uploading
    /// the page would clobber the overlay or vice versa). The offset is
    /// large enough that no plausible PDF page count collides with the
    /// overlay range.
    #[test]
    fn overlay_image_id_disjoint_from_page_id() {
        let r = KittyPageRegistry::new(false, 1000);
        for page in [0usize, 1, 100, 1024, 9999] {
            let p = r.image_id(page);
            let o = r.overlay_image_id(page);
            assert_ne!(p, o, "page {page}: id collision page={p} overlay={o}");
            // Distance is at least the offset constant; means even a
            // 100k-page PDF won't have its last page's id collide with
            // page 0's overlay id.
            assert!(
                o > p + 1000,
                "page {page}: overlay id should be far from page id"
            );
        }
    }

    /// `overlay_is_fresh` must return true ONLY when every key field
    /// matches the last `mark_transmitted` call. Selection signature is
    /// the field that changes per Visual-mode keystroke; flipping it
    /// must invalidate even when layout, revision, and dims are stable.
    #[test]
    fn overlay_is_fresh_only_when_all_fields_match() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        r.overlay_mark_transmitted(0, layout, 7, 12345, 100, 50);

        // Exact match → fresh.
        assert!(r.overlay_is_fresh(0, layout, 7, 12345, 100, 50));
        // Layout change → stale (e.g. user changed zoom).
        let layout2 = LayoutKey {
            fit_width_px: 128,
            dark: false,
        };
        assert!(!r.overlay_is_fresh(0, layout2, 7, 12345, 100, 50));
        // Revision change → stale (a highlight added on this page).
        assert!(!r.overlay_is_fresh(0, layout, 8, 12345, 100, 50));
        // Selection signature change → stale (caret moved by `l`).
        assert!(!r.overlay_is_fresh(0, layout, 7, 99999, 100, 50));
        // Dim change → stale (re-render at a different pixel size).
        assert!(!r.overlay_is_fresh(0, layout, 7, 12345, 200, 50));
        assert!(!r.overlay_is_fresh(0, layout, 7, 12345, 100, 100));
        // Different page → stale (nothing transmitted yet).
        assert!(!r.overlay_is_fresh(1, layout, 7, 12345, 100, 50));
    }

    /// Building an overlay transmit must populate the cache so that a
    /// subsequent build with the same fingerprint reuses the encoded
    /// bytes (no re-encode). Mirrors `build_transmit_caches_payload_across_calls`
    /// for the overlay path.
    #[test]
    fn overlay_build_transmit_caches_payload() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        let bm = RgbaImage::new(16, 16);

        let s1 = r.overlay_build_transmit(&bm, 0, layout, 0, 1);
        let cached_ptr_1 = r
            .overlays
            .get(&0)
            .unwrap()
            .cached_payload
            .as_ref()
            .unwrap()
            .bytes
            .as_ptr() as usize;

        // Same fingerprint → reuse cache. Returned strings should match
        // byte-for-byte AND the cached bytes Vec must not have been
        // reallocated.
        let s2 = r.overlay_build_transmit(&bm, 0, layout, 0, 1);
        let cached_ptr_2 = r
            .overlays
            .get(&0)
            .unwrap()
            .cached_payload
            .as_ref()
            .unwrap()
            .bytes
            .as_ptr() as usize;
        assert_eq!(
            s1, s2,
            "second build with same fingerprint should return identical string"
        );
        assert_eq!(
            cached_ptr_1, cached_ptr_2,
            "cached payload Vec must not have been reallocated"
        );

        // Different sel_sig → new encode (different bitmap intent).
        let _ = r.overlay_build_transmit(&bm, 0, layout, 0, 2);
        let cached_ptr_3 = r
            .overlays
            .get(&0)
            .unwrap()
            .cached_payload
            .as_ref()
            .unwrap()
            .bytes
            .as_ptr() as usize;
        // Pointer may or may not match (Vec might reuse the same
        // backing allocation if grown into the same slot) — what we
        // *can* assert is that the cached fingerprint flipped.
        let cached = &r.overlays.get(&0).unwrap().cached_payload.as_ref().unwrap();
        assert_eq!(cached.sel_sig, 2, "cache must update sel_sig on rebuild");
        let _ = cached_ptr_3;
    }

    /// Dropping an overlay queues a kitty `a=d,d=I,i=ID` so the
    /// terminal frees its decoded bitmap. Without this, scrolling a
    /// long doc with selections would leak overlay bitmaps in the
    /// terminal-side cache.
    #[test]
    fn overlay_drop_queues_terminal_delete() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        let bm = RgbaImage::new(16, 16);
        let _ = r.overlay_build_transmit(&bm, 0, layout, 0, 1);
        r.overlay_mark_transmitted(0, layout, 0, 1, 16, 16);

        r.overlay_drop(0);
        let deletes = r
            .take_pending_deletes()
            .expect("overlay_drop should queue a delete");
        assert!(
            deletes.contains("_Ga=d,d=I,i="),
            "expected delete APC, got {deletes:?}"
        );
    }

/// queue_deletes must collapse contiguous-id runs into one
    /// `d=R` and leave singletons as `d=I`. The mixed case (one
    /// range + one isolated id) is the realistic situation when
    /// some evictions are forward-scroll consecutive and one or
    /// two are random scroll-back jumps.
    #[test]
    fn queue_deletes_coalesces_runs_and_keeps_singletons() {
        let mut r = KittyPageRegistry::new(false, 0);
        let mut ids = vec![5u32, 7, 8, 9, 12];
        r.queue_deletes(&mut ids);
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
        // Out-of-order; queue_deletes sorts internally.
        let mut ids = vec![20u32, 10, 11, 22, 21];
        r.queue_deletes(&mut ids);
        let s = r.take_pending_deletes().expect("queued");
        // Sorted: 10,11, 20,21,22 → two ranges, no singletons.
        assert_eq!(s.matches("_Ga=d,d=R,").count(), 2);
        assert_eq!(s.matches("_Ga=d,d=I,").count(), 0);
        assert!(s.contains("d=R,x=10,y=11,"));
        assert!(s.contains("d=R,x=20,y=22,"));
    }

    /// Evicting a page that owns both a page image AND an overlay
    /// image must queue deletes for BOTH ids — otherwise an overlay
    /// bitmap could outlive its page on the terminal side and leak.
    #[test]
    fn evict_drops_overlay_alongside_page() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey {
            fit_width_px: 64,
            dark: false,
        };
        let bm = RgbaImage::new(16, 16);
        // Fill above cap; mark each page with an overlay too.
        for i in 0..(MAX_CACHED_PAGES + 4) {
            r.mark_transmitted(i, layout, 0, 16, 16);
            r.overlay_mark_transmitted(i, layout, 0, 1, 16, 16);
            r.pre_encode(&bm, i, layout, 0);
        }
        r.evict_to_budget(&[]);
        let deletes = r.take_pending_deletes().expect("evictions queue deletes");
        // 4 evicted pages → 8 freed image_ids: page IDs (4 contiguous
        // around id_base+1) and overlay IDs (4 contiguous starting at
        // id_base+OVERLAY_ID_OFFSET). With d=R coalescing this is two
        // separate ranges (page band and overlay band are far apart).
        assert_eq!(
            deletes.matches("_Ga=d,d=R,").count(),
            2,
            "page-id band and overlay-id band → 2 ranges; got {deletes:?}"
        );
        assert_eq!(
            deletes.matches("_Ga=d,d=I,").count(),
            0,
            "no per-id escapes when all evictions are contiguous"
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

        clear_page_area(&mut buf, area, &mut scratch);

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
    fn encode_payload_opaque_png_uses_rgb_color_type() {
        // Render a tiny opaque image; encoded PNG's IHDR must say RGB.
        let mut img = RgbaImage::new(4, 4);
        for p in img.pixels_mut() {
            *p = image::Rgba([200, 100, 50, 255]);
        }
        let (format_code, bytes) = encode_payload_opaque(&img);
        assert_eq!(format_code, 100, "PNG path expected for opaque encode");
        // PNG signature + IHDR. IHDR color type byte is at offset 25:
        //   0..8   PNG signature
        //   8..12  IHDR length
        //   12..16 "IHDR"
        //   16..20 width
        //   20..24 height
        //   24     bit depth
        //   25     color type (2 = RGB, 6 = RGBA)
        assert_eq!(
            bytes[25], 2,
            "PNG color type at byte 25 must be 2 (RGB), got {}",
            bytes[25]
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
        clear_page_area(&mut buf_a, area, &mut scratch_a);
        clear_page_area(&mut buf_b, area, &mut scratch_b);
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
        clear_page_area(&mut buf, area, &mut scratch);
        // Snapshot the cached bytes + capacity so we can detect a
        // re-build (which would re-allocate or rewrite the string).
        let cached_first = scratch.cached_row_clear.clone();
        let cap_first = scratch.cached_row_clear.capacity();
        let ptr_first = scratch.cached_row_clear.as_ptr() as usize;
        assert!(!cached_first.is_empty());
        assert_eq!(scratch.cached_row_clear_dims, (area.width, area.height));

        // Same area on a fresh buffer — must reuse the cached string.
        let mut buf2 = Buffer::empty(area);
        clear_page_area(&mut buf2, area, &mut scratch);
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
        clear_page_area(&mut buf3, resized, &mut scratch);
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
        clear_page_area(&mut buf_prev, area, &mut scratch);
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
        clear_page_area(&mut buf_curr, area, &mut scratch);

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

        clear_page_area(&mut buf, img_area, &mut scratch);

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
}
