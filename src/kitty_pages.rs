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
/// encoded payload (~250 KB PNG). 64 entries ≈ 16 MB in our process;
/// the terminal-side decoded RGBA is much larger (~5 MB/page) but
/// each eviction emits an `a=d,d=I,i=ID` delete so the terminal can
/// free its copy too. Without this cap, opening a 700-page PDF
/// would leak ~3.5 GB of decoded image into the terminal.
const MAX_CACHED_PAGES: usize = 64;

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
    /// Kitty `a=d,d=I,i=ID` delete escapes accumulated during eviction.
    /// Caller drains via `take_pending_deletes()` and prepends to the
    /// next transmit (or a synthetic one) so the terminal frees its
    /// decoded RGBA copy of the evicted image.
    pending_deletes: String,
}

impl KittyPageRegistry {
    pub fn new(is_tmux: bool, id_base: u32) -> Self {
        Self {
            is_tmux,
            id_base,
            pages: HashMap::new(),
            lru: VecDeque::new(),
            pending_deletes: String::new(),
        }
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
        let victim_set: std::collections::HashSet<usize> =
            victims.iter().copied().collect();
        for v in &victims {
            if let Some(entry) = self.pages.remove(v) {
                self.queue_delete(entry.image_id);
            }
        }
        self.lru.retain(|p| !victim_set.contains(p));
    }

    fn queue_delete(&mut self, id: u32) {
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
                let (format_code, bytes) = encode_payload(bitmap);
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
        let (format_code, bytes) = encode_payload(bitmap);
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
    let force_raw = std::env::var("TERMPDF_TRANSMIT_RAW")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    if force_raw {
        return (32, bitmap.as_raw().to_vec());
    }
    match encode_png_fast(bitmap) {
        Ok(png) => (100, png),
        Err(_) => (32, bitmap.as_raw().to_vec()),
    }
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

    // Chunk size matches ratatui-image: 4096 base64 chars per chunk
    // → 3072 raw bytes per chunk. Both kitty and tmux passthrough
    // expect chunked DCS sequences.
    const CHARS_PER_CHUNK: usize = 4096;
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
    build_transmit_string(&payload, format_code, id, bitmap.width(), bitmap.height(), is_tmux)
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
    let encoder = PngEncoder::new_with_quality(
        &mut buf,
        CompressionType::Fast,
        FilterType::Up,
    );
    encoder.write_image(
        bitmap.as_raw(),
        bitmap.width(),
        bitmap.height(),
        image::ExtendedColorType::Rgba8,
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
    let id_color = format!("\x1b[38;2;{id_r};{id_g};{id_b}m");
    let id_extra_diacritic = diacritic(u16::from(id_extra));

    // Reused string for each row's symbol. ratatui-image opts to write
    // the whole row's escape into the first cell + skip the rest;
    // we follow the same pattern for the same reason — ratatui's diff
    // would otherwise overwrite our placeholders with default cells.
    let cols = (width_cells as u32).min(MAX_COLS) as u16;
    let row_diacritics: String =
        std::iter::repeat_n('\u{10EEEE}', cols.saturating_sub(1) as usize).collect();
    let restore_cursor = format!(
        "\x1b[u\x1b[{}C\x1b[{}B",
        area.width.saturating_sub(1),
        area.height.saturating_sub(1)
    );

    let mut symbol = String::with_capacity(2048);
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
        write!(
            symbol,
            "\x1b[s{id_color}\u{10EEEE}{}{}{}",
            diacritic(img_row),
            diacritic(src_left_cell),
            id_extra_diacritic,
        )
        .unwrap();
        symbol.push_str(&row_diacritics);
        symbol.push_str(&restore_cursor);

        let cell_y = area.top().saturating_add(dst_top_cell.saturating_add(dy));
        if cell_y >= area.bottom() {
            break;
        }
        if let Some(cell) = buf.cell_mut((area.left(), cell_y)) {
            cell.set_symbol(&symbol);
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

fn tmux_wrap(is_tmux: bool) -> (&'static str, &'static str, &'static str) {
    if is_tmux {
        ("\x1bPtmux;", "\x1b\x1b", "\x1b\\")
    } else {
        ("", "\x1b", "")
    }
}

#[inline]
fn diacritic(n: u16) -> char {
    *DIACRITICS
        .get(usize::from(n))
        .unwrap_or(&DIACRITICS[0])
}

const MAX_COLS: u32 = DIACRITICS.len() as u32;

/// Kitty unicode-placeholder diacritics — copied verbatim from
/// <https://sw.kovidgoyal.net/kitty/_downloads/1792bad15b12979994cd6ecc54c967a6/rowcolumn-diacritics.txt>.
/// 297 entries cover image grids up to 297×297 cells, which is
/// comfortably more than any viewport.
static DIACRITICS: [char; 297] = [
    '\u{305}', '\u{30D}', '\u{30E}', '\u{310}', '\u{312}', '\u{33D}', '\u{33E}', '\u{33F}',
    '\u{346}', '\u{34A}', '\u{34B}', '\u{34C}', '\u{350}', '\u{351}', '\u{352}', '\u{357}',
    '\u{35B}', '\u{363}', '\u{364}', '\u{365}', '\u{366}', '\u{367}', '\u{368}', '\u{369}',
    '\u{36A}', '\u{36B}', '\u{36C}', '\u{36D}', '\u{36E}', '\u{36F}', '\u{483}', '\u{484}',
    '\u{485}', '\u{486}', '\u{487}', '\u{592}', '\u{593}', '\u{594}', '\u{595}', '\u{597}',
    '\u{598}', '\u{599}', '\u{59C}', '\u{59D}', '\u{59E}', '\u{59F}', '\u{5A0}', '\u{5A1}',
    '\u{5A8}', '\u{5A9}', '\u{5AB}', '\u{5AC}', '\u{5AF}', '\u{5C4}', '\u{610}', '\u{611}',
    '\u{612}', '\u{613}', '\u{614}', '\u{615}', '\u{616}', '\u{617}', '\u{657}', '\u{658}',
    '\u{659}', '\u{65A}', '\u{65B}', '\u{65D}', '\u{65E}', '\u{6D6}', '\u{6D7}', '\u{6D8}',
    '\u{6D9}', '\u{6DA}', '\u{6DB}', '\u{6DC}', '\u{6DF}', '\u{6E0}', '\u{6E1}', '\u{6E2}',
    '\u{6E4}', '\u{6E7}', '\u{6E8}', '\u{6EB}', '\u{6EC}', '\u{730}', '\u{732}', '\u{733}',
    '\u{735}', '\u{736}', '\u{73A}', '\u{73D}', '\u{73F}', '\u{740}', '\u{741}', '\u{743}',
    '\u{745}', '\u{747}', '\u{749}', '\u{74A}', '\u{7EB}', '\u{7EC}', '\u{7ED}', '\u{7EE}',
    '\u{7EF}', '\u{7F0}', '\u{7F1}', '\u{7F3}', '\u{816}', '\u{817}', '\u{818}', '\u{819}',
    '\u{81B}', '\u{81C}', '\u{81D}', '\u{81E}', '\u{81F}', '\u{820}', '\u{821}', '\u{822}',
    '\u{823}', '\u{825}', '\u{826}', '\u{827}', '\u{829}', '\u{82A}', '\u{82B}', '\u{82C}',
    '\u{82D}', '\u{951}', '\u{953}', '\u{954}', '\u{F82}', '\u{F83}', '\u{F86}', '\u{F87}',
    '\u{135D}', '\u{135E}', '\u{135F}', '\u{17DD}', '\u{193A}', '\u{1A17}', '\u{1A75}',
    '\u{1A76}', '\u{1A77}', '\u{1A78}', '\u{1A79}', '\u{1A7A}', '\u{1A7B}', '\u{1A7C}',
    '\u{1B6B}', '\u{1B6D}', '\u{1B6E}', '\u{1B6F}', '\u{1B70}', '\u{1B71}', '\u{1B72}',
    '\u{1B73}', '\u{1CD0}', '\u{1CD1}', '\u{1CD2}', '\u{1CDA}', '\u{1CDB}', '\u{1CE0}',
    '\u{1DC0}', '\u{1DC1}', '\u{1DC3}', '\u{1DC4}', '\u{1DC5}', '\u{1DC6}', '\u{1DC7}',
    '\u{1DC8}', '\u{1DC9}', '\u{1DCB}', '\u{1DCC}', '\u{1DD1}', '\u{1DD2}', '\u{1DD3}',
    '\u{1DD4}', '\u{1DD5}', '\u{1DD6}', '\u{1DD7}', '\u{1DD8}', '\u{1DD9}', '\u{1DDA}',
    '\u{1DDB}', '\u{1DDC}', '\u{1DDD}', '\u{1DDE}', '\u{1DDF}', '\u{1DE0}', '\u{1DE1}',
    '\u{1DE2}', '\u{1DE3}', '\u{1DE4}', '\u{1DE5}', '\u{1DE6}', '\u{1DFE}', '\u{20D0}',
    '\u{20D1}', '\u{20D4}', '\u{20D5}', '\u{20D6}', '\u{20D7}', '\u{20DB}', '\u{20DC}',
    '\u{20E1}', '\u{20E7}', '\u{20E9}', '\u{20F0}', '\u{2CEF}', '\u{2CF0}', '\u{2CF1}',
    '\u{2DE0}', '\u{2DE1}', '\u{2DE2}', '\u{2DE3}', '\u{2DE4}', '\u{2DE5}', '\u{2DE6}',
    '\u{2DE7}', '\u{2DE8}', '\u{2DE9}', '\u{2DEA}', '\u{2DEB}', '\u{2DEC}', '\u{2DED}',
    '\u{2DEE}', '\u{2DEF}', '\u{2DF0}', '\u{2DF1}', '\u{2DF2}', '\u{2DF3}', '\u{2DF4}',
    '\u{2DF5}', '\u{2DF6}', '\u{2DF7}', '\u{2DF8}', '\u{2DF9}', '\u{2DFA}', '\u{2DFB}',
    '\u{2DFC}', '\u{2DFD}', '\u{2DFE}', '\u{2DFF}', '\u{A66F}', '\u{A67C}', '\u{A67D}',
    '\u{A6F0}', '\u{A6F1}', '\u{A8E0}', '\u{A8E1}', '\u{A8E2}', '\u{A8E3}', '\u{A8E4}',
    '\u{A8E5}', '\u{A8E6}', '\u{A8E7}', '\u{A8E8}', '\u{A8E9}', '\u{A8EA}', '\u{A8EB}',
    '\u{A8EC}', '\u{A8ED}', '\u{A8EE}', '\u{A8EF}', '\u{A8F0}', '\u{A8F1}', '\u{AAB0}',
    '\u{AAB2}', '\u{AAB3}', '\u{AAB7}', '\u{AAB8}', '\u{AABE}', '\u{AABF}', '\u{AAC1}',
    '\u{FE20}', '\u{FE21}', '\u{FE22}', '\u{FE23}', '\u{FE24}', '\u{FE25}', '\u{FE26}',
    '\u{10A0F}', '\u{10A38}', '\u{1D185}', '\u{1D186}', '\u{1D187}', '\u{1D188}', '\u{1D189}',
    '\u{1D1AA}', '\u{1D1AB}', '\u{1D1AC}', '\u{1D1AD}', '\u{1D242}', '\u{1D243}', '\u{1D244}',
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
        let layout = LayoutKey { fit_width_px: 64, dark: false };
        assert!(!r.is_fresh(0, layout, 7, 64, 32));
        r.mark_transmitted(0, layout, 7, 64, 32);
        assert!(r.is_fresh(0, layout, 7, 64, 32));
    }

    #[test]
    fn revision_change_marks_stale() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey { fit_width_px: 64, dark: false };
        r.mark_transmitted(0, layout, 7, 64, 32);
        assert!(r.is_fresh(0, layout, 7, 64, 32));
        // Bumping revision (e.g. user moved selection) → not fresh.
        assert!(!r.is_fresh(0, layout, 8, 64, 32));
    }

    #[test]
    fn layout_change_marks_stale() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let l1 = LayoutKey { fit_width_px: 64, dark: false };
        let l2 = LayoutKey { fit_width_px: 64, dark: true };
        r.mark_transmitted(0, l1, 0, 64, 32);
        assert!(!r.is_fresh(0, l2, 0, 64, 32));
    }

    #[test]
    fn dimension_change_marks_stale() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey { fit_width_px: 64, dark: false };
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
        let layout = LayoutKey { fit_width_px: 64, dark: false };
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
        let layout = LayoutKey { fit_width_px: 64, dark: false };
        let bm = RgbaImage::new(16, 16);
        r.build_transmit(&bm, 0, layout, 7);
        let len_before = r.pages.get(&0).unwrap().cached_payload.as_ref().unwrap().bytes.len();
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
        let layout = LayoutKey { fit_width_px: 64, dark: false };
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
        let layout = LayoutKey { fit_width_px: 64, dark: false };
        let bm = RgbaImage::new(16, 16);
        r.pre_encode(&bm, 0, layout, 7);
        let ptr_after_pre = r.pages.get(&0).unwrap().cached_payload.as_ref().unwrap().bytes.as_ptr() as usize;
        let _s = r.build_transmit(&bm, 0, layout, 7);
        let ptr_after_transmit = r.pages.get(&0).unwrap().cached_payload.as_ref().unwrap().bytes.as_ptr() as usize;
        assert_eq!(
            ptr_after_pre, ptr_after_transmit,
            "build_transmit after pre_encode must reuse the bytes the pre-encode produced"
        );
    }

    #[test]
    fn evict_caps_at_max() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey { fit_width_px: 64, dark: false };
        let bm = RgbaImage::new(16, 16);
        // Fill past cap.
        for i in 0..(MAX_CACHED_PAGES + 8) {
            r.mark_transmitted(i, layout, 0, 16, 16);
            // Also stash a payload so eviction frees something visible.
            r.pre_encode(&bm, i, layout, 0);
        }
        r.evict_to_budget(&[]);
        assert_eq!(r.pages.len(), MAX_CACHED_PAGES);
        // Pending deletes should reference the 8 evicted ids.
        let deletes = r.take_pending_deletes().expect("evictions queued deletes");
        // 8 eviction events, each emits one `_Ga=d,d=I,i=...` blob.
        assert_eq!(deletes.matches("_Ga=d,d=I,i=").count(), 8);
    }

    #[test]
    fn evict_skips_pinned_visible() {
        let mut r = KittyPageRegistry::new(false, 1000);
        let layout = LayoutKey { fit_width_px: 64, dark: false };
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
        let layout = LayoutKey { fit_width_px: 64, dark: false };
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
        let mut buf = Buffer::empty(Rect { x: 0, y: 0, width: 10, height: 4 });
        let area = Rect { x: 0, y: 0, width: 10, height: 4 };
        let written = place_page(
            &mut buf,
            area,
            /*page_idx*/ 0,
            /*image_id*/ 1,
            /*pixel_w*/ 200,        // 20 cols at cell_w=10
            /*pixel_h*/ 80,         // 4 rows at cell_h=20
            /*cell_w_px*/ 10,
            /*cell_h_px*/ 20,
            /*dst_top_cell*/ 0,
            /*dst_height_cells*/ 4,
            /*src_top_cell*/ 0,
            /*src_left_cell*/ 5,
            /*width_cells*/ 10,
            /*prefix*/ None,
        );
        assert!(written > 0);
        let symbol = buf.cell((0, 0)).unwrap().symbol().to_string();
        // First diacritic = row(0), second = col(5), third = id_extra(0).
        let want_col = diacritic(5);
        let want_row = diacritic(0);
        assert!(
            symbol.contains(want_col),
            "first cell symbol must encode col=5 (= {:?}); got {:?}",
            want_col, symbol
        );
        assert!(symbol.contains(want_row), "first cell must still encode row=0");
    }

    /// When src_left_cell would point past the rightmost image column,
    /// width_cells is clamped so we don't emit placeholders that
    /// reference invalid image grid positions (kitty would show
    /// garbage / repeated content).
    #[test]
    fn place_page_clamps_width_at_image_right_edge() {
        let mut buf = Buffer::empty(Rect { x: 0, y: 0, width: 10, height: 4 });
        let area = Rect { x: 0, y: 0, width: 10, height: 4 };
        // 12-col-wide image; src_left_cell=8 leaves only 4 valid cols.
        let written = place_page(
            &mut buf,
            area,
            0,
            1,
            /*pixel_w*/ 120,
            /*pixel_h*/ 80,
            10, 20,
            0, 4, 0, /*src_left_cell*/ 8, /*requested width*/ 10,
            None,
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
}
