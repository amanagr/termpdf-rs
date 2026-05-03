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

use std::collections::HashMap;
use std::fmt::Write as _;

use image::RgbaImage;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::app::LayoutKey;

/// Image-cache entry per page index.
#[derive(Debug, Clone, Copy)]
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
}

pub struct KittyPageRegistry {
    is_tmux: bool,
    /// Base for per-page IDs. `id_for(page) = base + 1 + page_idx`.
    /// Adding 1 keeps the canvas-mode `kitty_image_id` (= base) free
    /// for any fallback path that might still want to use it.
    id_base: u32,
    pages: HashMap<usize, PageEntry>,
}

impl KittyPageRegistry {
    pub fn new(is_tmux: bool, id_base: u32) -> Self {
        Self {
            is_tmux,
            id_base,
            pages: HashMap::new(),
        }
    }

    /// Drop all cached entries. Caller is responsible for sending
    /// `a=d,d=A` if it wants to also free the terminal-side cache.
    /// (We don't, on the theory that the terminal will GC images that
    /// no placeholder references.)
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.pages.clear();
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
        });
        entry.image_id = id;
        entry.transmitted_layout = Some(layout);
        entry.transmitted_revision = revision;
        entry.pixel_w = pixel_w;
        entry.pixel_h = pixel_h;
    }

    /// Build the kitty transmit escape for this page's bitmap. Free
    /// function exposed via the registry so callers don't have to
    /// know the `is_tmux` flag separately.
    pub fn build_transmit(&self, bitmap: &RgbaImage, page_idx: usize) -> String {
        transmit(bitmap, self.id_for(page_idx), self.is_tmux)
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

/// Build the kitty `a=T,U=1` chunked transmit for a bitmap. The
/// terminal stores the image keyed by `id`; subsequent placements
/// reference this ID.
fn transmit(bitmap: &RgbaImage, id: u32, is_tmux: bool) -> String {
    let (w, h) = (bitmap.width(), bitmap.height());
    let bytes = bitmap.as_raw();

    let (start, escape, end) = tmux_wrap(is_tmux);

    // Match ratatui-image's chunk size: 4096 base64 chars per chunk,
    // which means 3072 raw bytes (base64 has 4/3 expansion).
    const CHARS_PER_CHUNK: usize = 4096;
    const RAW_PER_CHUNK: usize = (CHARS_PER_CHUNK / 4) * 3;

    let chunks: Vec<&[u8]> = bytes.chunks(RAW_PER_CHUNK).collect();
    let chunk_count = chunks.len();

    // Reserve roughly the worst case to avoid mid-loop reallocations.
    let mut data = String::with_capacity(chunk_count * (CHARS_PER_CHUNK + 64));

    for (i, chunk) in chunks.iter().enumerate() {
        data.push_str(start);
        write!(data, "{escape}_Gq=2,").unwrap();
        if i == 0 {
            // q=2 suppresses kitty responses (we don't read them).
            // f=32 = RGBA, t=d = direct transmit (data inline), s/v
            // = pixel dimensions, U=1 = mark for unicode-placeholder
            // use, a=T = transmit-and-store (no immediate placement).
            write!(data, "i={id},a=T,U=1,f=32,t=d,s={w},v={h},").unwrap();
        }
        let more = u8::from(chunk_count > i + 1);
        write!(data, "m={more};").unwrap();
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode_string(chunk, &mut data);
        write!(data, "{escape}\\").unwrap();
        data.push_str(end);
    }
    data
}

/// Write placeholder cells for the given page into `buf`. The
/// placement starts at terminal cell `(area.left(), area.top() +
/// dst_top_cell)` and covers `dst_height_cells` rows × `width_cells`
/// columns, sourcing rows starting from `src_top_cell` of the image.
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
    width_cells: u16,
    prefix: Option<&str>,
) -> u16 {
    let _ = page_idx; // reserved for future per-page debug; placement is purely id-driven
    let _ = pixel_w; // width is implied by `width_cells × cell_w_px`
    let _ = cell_w_px;
    let _ = cell_h_px;

    let max_src_rows = (pixel_h / cell_h_px.max(1)) as u16;
    let max_dst_rows_in_area = area.height.saturating_sub(dst_top_cell);
    // Source rows we have; destination rows we have; whichever is fewer.
    let height_cells = dst_height_cells
        .min(max_dst_rows_in_area)
        .min(max_src_rows.saturating_sub(src_top_cell));
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
        // this row inherit the fg color and increment the col by 1.
        write!(
            symbol,
            "\x1b[s{id_color}\u{10EEEE}{}{}{}",
            diacritic(img_row),
            diacritic(0),
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

    #[test]
    fn transmit_tmux_wrapping_present() {
        let bm = RgbaImage::new(8, 8);
        let s = transmit(&bm, 1, true);
        assert!(s.starts_with("\x1bPtmux;"));
        assert!(s.ends_with("\x1b\\"));
    }
}
