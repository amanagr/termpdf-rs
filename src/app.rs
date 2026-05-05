use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use image::{DynamicImage, Rgba, RgbaImage};
use pdfium_render::prelude::PdfDocument;
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::kitty::StatefulKitty;
use ratatui_image::protocol::{ImageSource, StatefulProtocol, StatefulProtocolType};

use crate::highlight::{Highlight, HighlightStore, Rect01, HIGHLIGHT_COLORS};
use crate::layout::PageLayout;
use crate::links::LinkAction;
use crate::outline::{self, OutlineEntry};
use crate::pdf::{self, PageMetrics};
use crate::pdfhighlights;
use crate::search::SearchResults;
use crate::session::Session;
use crate::textlayout::{Caret, SelMode, TextCache, TextSelection};

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Normal,
    Command,
    Visual,
    Search,
}

/// Cache key for the *layout + per-page render* tier. When any of
/// these changes, every cached page bitmap is stale and the layout
/// must be rebuilt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutKey {
    pub fit_width_px: u32,
    pub dark: bool,
}

/// One hint shown over a clickable link region. Rendered as a 1-2
/// char label by the UI; the action fires on disambiguation.
#[derive(Debug, Clone)]
pub struct HintEntry {
    pub page_idx: usize,
    /// Link rect in normalised page coords (origin top-left).
    pub rect: Rect01,
    /// What happens if the user picks this hint.
    pub action: LinkAction,
    /// 1-2 char label rendered over the link.
    pub label: String,
}

/// Cache key for the *highlights-baked* tier — page bitmap with
/// saved highlights and search hits alpha-blended in, but NO live
/// selection. Rebuilt only when highlights or search results change;
/// reused across every Visual-mode keystroke. The overlay tier
/// (which adds the live selection) clones from this instead of from
/// the raw page bitmap, so selection-only motions never re-blend
/// the saved-highlights list — a big win on heavily-highlighted
/// pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightsBakedKey {
    pub layout: LayoutKey,
    pub highlight_revision: u64,
    pub search_revision: u64,
    pub has_search_hits: bool,
    pub current_hit_on_this_page: bool,
}

/// Cache key for the *per-page overlay* tier. The composited
/// (with-overlays) RgbaImage cached in `overlay_cache` is keyed on
/// this so a mouse-drag selection only rebuilds the bitmap of the
/// page the selection lives on — everything else keeps its
/// already-overlaid copy across frames.
///
/// `selection_sig` is the selection's per-page fingerprint (or 0 if
/// the selection doesn't touch this page). The live Visual-mode band
/// is alpha-blended into the page bitmap because the cell-overlay
/// approach we tried first was unreliable in tmux-passthrough kitty:
/// ratatui-image's placeholder packing meant our cell writes never
/// reached the wire. Baking it into the bitmap goes through the same
/// kitty re-upload path that already works for saved highlights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageOverlayKey {
    pub layout: LayoutKey,
    pub highlight_revision: u64,
    /// Search-results revision (if any). Bumped when a new search
    /// runs, when the user advances to next/prev hit (so the "current
    /// hit" outline moves), or when results clear.
    pub search_revision: u64,
    /// Whether *this page* has any search hits — saves a per-frame
    /// scan of the hit list during compose when there are none.
    pub has_search_hits: bool,
    /// True if the currently-focused search hit lives on this page.
    pub current_hit_on_this_page: bool,
    /// Fingerprint of the active selection's contribution to this
    /// page (0 = none). Hash of (lo_idx, hi_idx, mode, color_idx),
    /// stable across frames that don't change the selection.
    pub selection_sig: u64,
}

/// Cache key for the *compose* tier (stitch visible pages into a
/// viewport-sized canvas, blend overlays). Cheap; we still cache it
/// so a still frame doesn't pointlessly re-blit. `selection_sig` is
/// a process-global fingerprint of the active selection so the
/// compose cache invalidates when it moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComposeKey {
    pub layout: LayoutKey,
    pub viewport_w: u32,
    pub viewport_h: u32,
    pub scroll_y_px: i64,
    pub scroll_x_milli: u32,
    pub highlight_revision: u64,
    pub selection_sig: u64,
    /// Search-results revision (bumped by `n`/`N`/`/`-edit/clear).
    /// Without this field the canvas-mode tier would only invalidate
    /// via the explicit `invalidate_compose()` calls in `advance_search`
    /// and friends — load-bearing. Any future code path that mutates
    /// `app.search` directly would silently leave the canvas tier
    /// painting the prior current-hit outline. Per-page kitty
    /// revision (`compute_page_revision`) already mixes search; this
    /// brings the compose tier in line.
    pub search_revision: u64,
}

pub struct App<'doc> {
    pub document: PdfDocument<'doc>,
    pub path: PathBuf,
    pub page_count: usize,
    pub page_metrics: Vec<PageMetrics>,

    pub dark: bool,
    pub mode: Mode,
    pub pending: String,    // numeric prefix typed in normal mode
    pub cmd_buffer: String, // text typed after `:` or `/`
    pub status: String,     // ephemeral status-line message
    pub show_help: bool,
    pub zoom: f32,

    /// Vertical scroll position in pixels from the top of the
    /// continuous document. `0` is doc-top; clamped at
    /// `total_height - viewport_h`.
    pub scroll_y_px: i64,
    /// Horizontal scroll, 0..=1 over (fit_width_px - viewport_w_px).
    /// Only meaningful when zoom > 1 (page wider than viewport).
    pub scroll_x: f32,

    /// Set every frame by `ui::ensure_image`. Visual-mode key handler
    /// uses these to convert "screen-fraction" deltas into
    /// page-relative ones, and `dispatch_mouse` uses them to map
    /// terminal cells back to page coordinates.
    pub viewport_px: (u32, u32),
    pub image_area: Rect,
    pub cell_size_px: (u16, u16),
    /// Current `PageLayout` matching the cached page bitmaps. Built
    /// from `page_metrics` whenever `LayoutKey` changes.
    pub layout: PageLayout,
    pub last_layout_key: Option<LayoutKey>,

    /// Vim-style text selection: anchor + head carets pointing at
    /// chars in the document's text layer. `None` outside Visual
    /// mode. The carets address pages (page idx) and chars within a
    /// page (idx into the page's `PageText.chars`). `text_cache`
    /// holds the per-page char geometry these carets reference.
    pub text_selection: Option<TextSelection>,
    pub selection_color_idx: usize,
    /// True while the user is positioning the caret BEFORE growing a
    /// selection. In this mode each motion moves both `anchor` and
    /// `head` together so the band stays a single char (placement,
    /// not selection). Pressing `v` again toggles to selection mode:
    /// anchor locks and motions only move `head`. Toggle back with
    /// `v` if the user wants to relocate the start point.
    pub selection_placement: bool,
    /// Per-page text-layout cache, lazily populated when the user
    /// enters Visual mode or starts a mouse drag. LRU-evicted along
    /// with the page bitmap cache.
    pub text_cache: TextCache,
    /// True while a mouse drag is in progress — `mouse_drag_to` only
    /// updates `text_selection.head` when this is set.
    pub mouse_dragging: bool,

    pub picker: Picker,
    /// Stable per-process kitty image ID. Every kitty graphics
    /// transmission reuses this ID so the terminal *replaces* the
    /// previous image data instead of appending a fresh image to
    /// its store. Without this, every scroll/zoom/highlight change
    /// allocates a new image in the terminal's memory; Ghostty
    /// crashed at ~10–20 such allocations because ratatui-image's
    /// default `new_resize_protocol` randomises the ID per call.
    pub kitty_image_id: u32,
    /// Cached `is_tmux` flag — picker's own field is private, but
    /// the same `$TMUX` heuristic ratatui-image uses is good enough
    /// for kitty passthrough wrapping.
    pub is_tmux: bool,
    /// Per-page rendered bitmap (no overlays applied). Bounded by
    /// both a sliding window around the visible range *and* a hard
    /// byte budget — whichever is tighter wins. Insertion order is
    /// tracked separately so we can LRU-evict when over budget.
    pub page_cache: HashMap<usize, DynamicImage>,
    /// Pages currently in `page_cache` at `RenderQuality::Fast` —
    /// they were rendered hot during a scroll and want a Sharp upgrade
    /// once the user is idle. Pages loaded from the disk cache or
    /// rendered at Sharp directly are NOT in this set, so the idle
    /// path can scan it cheaply to find upgrade candidates.
    /// See pdf::RenderQuality + main.rs::upgrade_one_visible_to_sharp.
    pub pages_at_fast_quality: std::collections::HashSet<usize>,
    /// LRU order — most-recently-used page is at the back. Touched
    /// every time `ensure_image` reads or inserts a page.
    pub page_cache_lru: Vec<usize>,
    /// Sign of the last vertical scroll (+1 / -1 / 0). Used to
    /// prefetch ahead of the user's scroll direction so steady
    /// reading rarely hits a cache miss.
    pub last_scroll_dir: i8,
    /// Page bitmaps that pdfium failed on (corrupt page,
    /// out-of-memory, …). Cached so we don't re-attempt every frame.
    pub failed_pages: std::collections::HashSet<usize>,
    /// Per-page bitmap with saved highlights and (if applicable)
    /// the active selection blended in. Rebuilt on overlay change
    /// for a single page; everything else stays cached. This is
    /// what the drag-time hot path reads — without it, every
    /// mouse-move event re-cloned every visible page.
    pub overlay_cache: HashMap<usize, (RgbaImage, PageOverlayKey)>,
    /// Per-page bitmap with saved highlights and search hits blended
    /// in — but NOT the live selection. Reused across every
    /// Visual-mode keystroke (selection_sig is intentionally absent
    /// from `HighlightsBakedKey`). On selection move we clone from
    /// here instead of from `page_cache`, skipping the per-highlight
    /// blend loop.
    pub highlights_baked_cache: HashMap<usize, (RgbaImage, HighlightsBakedKey)>,
    /// Background page-render worker (`None` if it failed to spawn —
    /// falls back to fully synchronous rendering). Used only for
    /// prefetch: visible-page rendering on cold cache stays sync so
    /// the user sees the new page in the same frame.
    pub render_worker: Option<crate::render_worker::RenderWorker>,
    /// (page, target_width_px, dark) tuples currently in flight on
    /// the worker. Prevents duplicate requests when the prefetch
    /// loop runs back-to-back.
    pub pages_in_flight: std::collections::HashSet<(usize, u32, bool)>,
    pub image_proto: Option<StatefulProtocol>,
    /// Per-page kitty placements registry. `Some(...)` only when the
    /// picker selected the kitty protocol; the canvas / ratatui-image
    /// path is used for sixel/iterm2/halfblocks. When set, the kitty
    /// draw path bypasses canvas composition entirely — each page is
    /// transmitted once with its own image ID and re-shown via
    /// unicode-placeholder cells. Drops the per-frame Draw cost from
    /// ~150 ms (full canvas re-encode + pty write) to ~5 ms.
    pub kitty_pages: Option<crate::kitty_pages::KittyPageRegistry>,
    /// Reusable scratch buffers for `ui::draw_pages_kitty` so the
    /// per-frame Vec allocations (visible, blits, transmits, etc.)
    /// drop to `clear()` calls between frames. Taken via
    /// `mem::take` at the top of the draw and restored at the bottom.
    pub draw_scratch: crate::ui::DrawScratch,
    /// Most recent input event time. Used by `is_rapid_scrolling` to
    /// detect a sustained autorepeat / mouse-wheel burst so the kitty
    /// draw path can defer cold-page transmits until the burst ends.
    /// `None` until the first event arrives.
    pub last_input_at: Option<std::time::Instant>,
    /// Count of consecutive inputs within the rapid-scroll window.
    /// A single isolated keypress hits count=1 → NOT considered
    /// rapid; the user wants their page to show immediately.
    /// Held-j autorepeats fire ~25 events/sec → count climbs fast.
    pub input_burst_count: u32,
    /// Timestamp of the last *applied* scroll keypress. Used by
    /// `note_scroll_attempt` to throttle held-key autorepeat: Linux
    /// keyboard autorepeat fires ~30 Hz, which would flip 30 PDF
    /// pages/sec, way faster than the user can read or any browser
    /// scrolls. The throttle accepts at most one scroll per
    /// `SCROLL_THROTTLE_MS` so a held `j` lands at a human-readable
    /// cadence.
    pub last_scroll_applied_at: Option<std::time::Instant>,
    /// Set by the kitty draw path when it deferred cold-page renders
    /// past its per-frame budget. The run-loop reads this, forces an
    /// immediate next-iteration draw, and clears it. Staggering
    /// catch-up renders one-per-frame keeps Ghostty's renderer from
    /// crashing on the multi-MB transmit burst that a big jump (e.g.
    /// `100G` on a 600-page book) used to dump in a single frame.
    pub pending_cold_redraw: bool,
    pub last_compose_key: Option<ComposeKey>,
    /// Inclusive `(lo, hi)` page range the previous compose's
    /// selection touched. Lets `try_selection_only_repaint` re-blit
    /// pages that *exited* the selection range — without this, the
    /// canvas-mode (sixel/iterm2/halfblocks) renderer would keep
    /// showing a stale selection band on a page the user just
    /// shrank past. The kitty path doesn't need it (each page
    /// transmits independently and `compute_page_revision` includes
    /// the per-page `selection_sig`).
    pub last_selection_range: Option<(usize, usize)>,
    /// Reused viewport-sized RGBA buffer. Allocating an 8 MB
    /// `RgbaImage::from_pixel` per recompose was 4–6 ms of pure
    /// allocator + memset on every j/k tick at 1080p. We hand the
    /// composer a pre-sized buffer instead and only touch the gap
    /// rows between pages.
    pub canvas_buf: Option<RgbaImage>,
    /// Pre-baked single row of background pixels (RGBA bytes), reused
    /// across `fill_gap_rows` and `try_scroll_shift_canvas` so we
    /// don't re-build a viewport_w * 4 byte Vec on every frame. Keyed
    /// by (viewport_w, dark) and rebuilt only when either changes.
    pub bg_row_buf: Vec<u8>,
    pub bg_row_key: Option<(u32, bool)>,
    /// FNV-1a hash of the most recently encoded canvas buffer.
    /// Used to skip the kitty re-encode when ComposeKey changed but
    /// the resulting pixels happen to match the previous frame
    /// (common when the selection moves to/from offscreen, when
    /// scrolling lands on a page boundary, or when the user mashes
    /// the same key past a layout edge). 0 means "nothing encoded
    /// yet."
    pub last_canvas_hash: u64,

    pub highlights: HighlightStore,
    /// Bumped on every highlight add/delete so the compose cache
    /// invalidates without re-hashing the store.
    pub highlight_revision: u64,
    /// Pages that had our highlight annotations on disk at load time.
    /// On save, we walk the union of this and the current store's
    /// per-page set: `prev` covers deletes (page that had ours but no
    /// longer does), `current` covers adds. Pages in neither set are
    /// guaranteed not to need work, so save_to_pdf_filtered can skip
    /// the per-page pdfium open. Saves ~5–7 s on a quit-after-edit on
    /// a 700-page book.
    pub prev_highlight_pages: std::collections::HashSet<usize>,

    /// Active search results, if any. `None` means no `/` query is
    /// in flight (or the user just `:nohl`d).
    pub search: Option<SearchResults>,
    /// Last-run query, restored when the user types `/` then Enter
    /// with an empty buffer (vim-style "redo last search").
    pub last_query: Option<String>,
    /// Lazy full-text index of the document. Populated one page per
    /// idle warm tick; consulted by `run_search` to skip pages that
    /// definitely don't contain the query. Sioyek-style win:
    /// 4 100-page docs go from ~5 s search to ~0.03 s once filled.
    pub doc_index: crate::search_index::DocIndex,
    /// Has the *complete* index been written to disk yet? Set once
    /// after the first `is_complete()` save so we don't re-write
    /// every frame. Reset implicitly on file-mtime change via the
    /// disk_cache hash key (loaded index won't be found).
    pub index_persisted: bool,

    /// Vimium-style link-follow state. `true` while the user has
    /// pressed `f` and is typing the hint chars to pick a target.
    pub link_hint_mode: bool,
    /// Hints offered in the current hint-mode session, populated when
    /// the user presses `f`. Empty when `link_hint_mode = false`.
    pub link_hints: Vec<HintEntry>,
    /// Chars typed so far during hint mode; narrows `link_hints` to
    /// the still-matching subset for incremental disambiguation.
    pub hint_filter: String,

    /// Document outline, eager-loaded once at startup. Empty Vec
    /// means "loaded, no outline" (vs `None` which would be "not
    /// yet loaded" — we avoid that state by loading in `App::new`).
    pub outline: Vec<OutlineEntry>,
    /// Sorted-deduped page indices of `outline` entries with a
    /// resolved page. Built once at construction; consulted on every
    /// `]]`/`[[` press to avoid an allocation + sort + dedup on the
    /// section-jump path.
    pub outline_pages_sorted: Vec<usize>,
    /// `true` while the TOC overlay panel is open. Behaves like
    /// `show_help` — consumed first by `dispatch` so existing
    /// keybindings stay intact.
    pub show_toc: bool,
    /// Index in `outline_filtered` of the currently-selected entry.
    pub toc_cursor: usize,
    /// Optional substring filter applied to `outline` to produce
    /// `outline_filtered_indices`. Empty = no filter.
    pub toc_filter: String,
    /// Memoised result of `toc_filtered_indices`: `(filter_string,
    /// matching_outline_indices)`. Refreshed only when `toc_filter`
    /// changes; same filter on the next draw is a single string-eq
    /// check instead of a fuzzy walk over the whole outline.
    pub toc_filtered_cache: Option<(String, Vec<usize>)>,
    /// Whether the user is currently editing `toc_filter` (started
    /// by `/` while the TOC is open).
    pub toc_filter_editing: bool,
    /// Set by `App::new` when the user passed a starting page (or the
    /// session restored one). Consumed by the first `ensure_layout`
    /// call to compute the initial scroll offset, then cleared.
    pub pending_initial_page: Option<usize>,
    /// Fraction `0..=1` into the pending initial page where the
    /// previous session left the user. Applied alongside
    /// `pending_initial_page` so reopens land mid-page exactly where
    /// the user was reading, not at the page boundary above.
    pub pending_initial_scroll_in_page: f32,

    /// Vim-style named marks `m{a..z}` → page index. Persisted via
    /// `Session` so a reopened document still has its marks.
    pub marks: std::collections::BTreeMap<char, usize>,
    /// Jumplist of recently-visited pages. `<C-o>` walks backwards,
    /// `<C-i>` (Tab) walks forward; same model as vim. Cursor sits
    /// at `jump_idx`.
    pub jumplist: Vec<usize>,
    pub jump_idx: usize,
    /// True after typing `m`, awaiting the mark name. Mirrors the
    /// `g`-pending pattern in `keys.rs`.
    pub awaiting_mark_set: bool,
    /// True after typing `'`, awaiting the mark name to jump to.
    pub awaiting_mark_jump: bool,

    /// Counter for new highlight `group_id`s. All rects from a single
    /// yank share one id so `x` can delete the whole group at once.
    /// Seeded from the highest existing group_id in the loaded store
    /// so reopening a doc doesn't recycle ids.
    pub next_highlight_group_id: u64,
    /// Two-stroke confirm for `x` (delete-highlight). When true, the
    /// next `y` keypress finalises the delete; any other key cancels.
    /// Cleared on every keypress that isn't the confirm itself.
    pub awaiting_highlight_delete_confirm: bool,

    /// Per-PDF disk-cache directory, resolved once at session start so
    /// `ensure_page_rendered` doesn't pay a stat() + env-scan per cold
    /// page render. `None` when the cache dir can't be determined.
    pub cache_dir: Option<PathBuf>,

    pub should_quit: bool,
}

/// Inter-input gap (ms) below which we count consecutive presses as
/// part of the same scroll burst. 250 ms — was 120 ms, which only
/// caught held-key autorepeat (~30 ms) and missed normal-cadence
/// tap-tap scrolling (200-300 ms). At the tap rate we'd pay a full
/// pdfium cold-render + PNG encode + pty transmit per keystroke and
/// the user reported sustained 35 W draw + 75-90 °C from this. 250 ms
/// covers held-key, mouse-wheel spam (50-200 ms), and aggressive
/// skim cadence; single isolated taps stay below the burst minimum
/// so the page they revealed still renders on the keystroke.
pub const RAPID_SCROLL_THRESHOLD_MS: u128 = 250;
/// Minimum gap (ms) between accepted scroll keypresses. Linux
/// keyboard autorepeat fires ~30 Hz; without throttle a held `j`
/// flips 30 PDF pages/sec — way faster than the user can read or
/// any browser scrolls. 150 ms = ~6.7 Hz max, which matches a
/// brisk reading cadence. Two scrolls inside the throttle window
/// drop the second one (the user can't visually process them
/// anyway). Single intentional taps further apart than 150 ms are
/// always honoured.
pub const SCROLL_THROTTLE_MS: u128 = 150;
/// Minimum consecutive inputs in the burst window before we treat the
/// scroll as rapid. =3 keeps a single isolated keypress from being
/// flagged as a burst (it would defer the only page the user wanted
/// to see).
pub const RAPID_SCROLL_BURST_MIN: u32 = 3;

impl<'doc> App<'doc> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        document: PdfDocument<'doc>,
        path: &Path,
        page: usize,
        dark: bool,
        zoom: f32,
        marks: std::collections::BTreeMap<char, usize>,
        scroll_in_page: f32,
        picker: Picker,
    ) -> Result<Self> {
        let page_count = document.pages().len() as usize;
        let page_metrics = pdf::page_metrics(&document)?;
        let outline = outline::load(&document).unwrap_or_else(|e| {
            eprintln!(
                "warning: could not load outline: {}",
                crate::term_safe::safe_for_stderr(&format!("{e:#}"))
            );
            Vec::new()
        });
        let outline_pages_sorted: Vec<usize> = {
            let mut v: Vec<usize> = outline
                .iter()
                .filter_map(|e| e.page)
                .filter(|p| *p < page_count)
                .collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        let highlights = pdfhighlights::load_from_pdf(&document).unwrap_or_else(|e| {
            eprintln!(
                "warning: could not read PDF annotations: {}",
                crate::term_safe::safe_for_stderr(&format!("{e:#}"))
            );
            HighlightStore::default()
        });
        // Snapshot of pages that had our annotations on disk. On save
        // we walk the union of this set and the current per-page set;
        // pages in neither are guaranteed untouched and can be skipped.
        let prev_highlight_pages: std::collections::HashSet<usize> =
            highlights.items.iter().map(|h| h.page).collect();
        // Seed the group-id counter past any existing id so a reopen
        // of a document we previously wrote can't reuse a value.
        // `checked_add` here so a hostile/corrupted PDF carrying a
        // `group_id == u64::MAX` in its `AnnotMeta` doesn't panic on
        // open. In that pathological case we fall back to 1, NOT 0:
        // if the original document also has a group_id == 0
        // highlight (likely, since 0 is the first id assigned in any
        // session that's ever yanked), the next-yank merge of new=0
        // and old=0 would let a single delete-confirm sweep both
        // groups in one keystroke. Starting fresh ids at 1 leaves
        // the existing 0-group alone.
        let next_highlight_group_id = highlights
            .items
            .iter()
            .filter_map(|h| h.group_id)
            .max()
            .and_then(|m| m.checked_add(1))
            .unwrap_or(1);
        // Empty layout — first `ensure_image` call builds a real one
        // once the viewport size is known.
        let layout = PageLayout::build(&[], 0, 0);
        // Resolve once: every cold-page render in `ensure_page_rendered`
        // also wants this dir, and the `pdf_cache_dir` lookup pays a
        // stat() + env-scan per call. Caching once at session start
        // skips both on the per-render hot path.
        let cache_dir = crate::disk_cache::pdf_cache_dir(path);
        // Try to load a previously-built search index from disk.
        // Hit saves ~5 ms × N pages of pdfium text extraction
        // (~3.5 s for a 700-page book on second open).
        let doc_index = cache_dir
            .as_ref()
            .map(|d| d.join("index.bin"))
            .and_then(|p| crate::search_index::load(&p, page_count))
            .unwrap_or_else(|| crate::search_index::DocIndex::new(page_count));
        let index_persisted = doc_index.is_complete();
        // Resolve once and reuse for both KittyPageRegistry init and the
        // App::is_tmux field. (Previously did std::env::var("TMUX") twice
        // — harmless at init, but the duplication invited drift.)
        let is_tmux = std::env::var("TMUX").is_ok();
        // Mint the stable id once and reuse for BOTH the registry's
        // id_base AND the standalone `kitty_image_id` (canvas-mode
        // path). Each `stable_kitty_id()` call mixes
        // `SystemTime::now()` so calling it twice would produce two
        // different ids — and a future code change that mixes the
        // canvas and registry paths could collide on the registry's
        // page-id range. Today they're mutually exclusive but the
        // contract `stable_kitty_id` claims is "process-stable id";
        // honour it.
        let kitty_id_seed = stable_kitty_id();
        let kitty_pages = matches!(
            picker.protocol_type(),
            ratatui_image::picker::ProtocolType::Kitty
        )
        .then(|| crate::kitty_pages::KittyPageRegistry::new(is_tmux, kitty_id_seed));
        let app = Self {
            document,
            path: path.to_path_buf(),
            page_count,
            page_metrics,
            dark,
            mode: Mode::Normal,
            pending: String::new(),
            cmd_buffer: String::new(),
            status: String::new(),
            show_help: false,
            zoom: zoom.clamp(0.25, 8.0),
            scroll_y_px: 0,
            scroll_x: 0.0,
            viewport_px: (0, 0),
            image_area: Rect::default(),
            cell_size_px: picker.font_size(),
            layout,
            last_layout_key: None,
            text_selection: None,
            selection_color_idx: 0,
            selection_placement: false,
            text_cache: TextCache::default(),
            mouse_dragging: false,
            picker,
            kitty_image_id: kitty_id_seed,
            is_tmux,
            page_cache: HashMap::new(),
            pages_at_fast_quality: std::collections::HashSet::new(),
            page_cache_lru: Vec::new(),
            last_scroll_dir: 0,
            failed_pages: std::collections::HashSet::new(),
            overlay_cache: HashMap::new(),
            highlights_baked_cache: HashMap::new(),
            render_worker: None, // populated by main after construction
            pages_in_flight: std::collections::HashSet::new(),
            image_proto: None,
            kitty_pages,
            draw_scratch: crate::ui::DrawScratch::default(),
            last_input_at: None,
            input_burst_count: 0,
            last_scroll_applied_at: None,
            pending_cold_redraw: false,
            canvas_buf: None,
            bg_row_buf: Vec::new(),
            bg_row_key: None,
            last_canvas_hash: 0,
            last_compose_key: None,
            last_selection_range: None,
            highlights,
            highlight_revision: 0,
            prev_highlight_pages,
            search: None,
            last_query: None,
            doc_index,
            index_persisted,
            link_hint_mode: false,
            link_hints: Vec::new(),
            hint_filter: String::new(),
            outline,
            outline_pages_sorted,
            show_toc: false,
            toc_cursor: 0,
            toc_filter: String::new(),
            toc_filtered_cache: None,
            toc_filter_editing: false,
            pending_initial_page: if page < page_count { Some(page) } else { None },
            pending_initial_scroll_in_page: scroll_in_page.clamp(0.0, 1.0),
            marks,
            jumplist: Vec::new(),
            jump_idx: 0,
            awaiting_mark_set: false,
            awaiting_mark_jump: false,
            next_highlight_group_id,
            awaiting_highlight_delete_confirm: false,
            cache_dir,
            should_quit: false,
        };
        Ok(app)
    }

    /// Rebuild the layout if `LayoutKey` changed. On the very first
    /// call (no prior layout), resolves any `pending_initial_page`
    /// into a real scroll offset. Called at the top of
    /// `ui::ensure_image`.
    pub fn ensure_layout(&mut self, fit_width_px: u32, viewport_h_px: u32) {
        let key = LayoutKey {
            fit_width_px,
            dark: self.dark,
        };
        if self.last_layout_key == Some(key) {
            // Layout unchanged — but viewport_h might have shrunk so
            // the current scroll could exceed the new max. Re-clamp.
            self.scroll_y_px = self.layout.clamp_scroll(self.scroll_y_px, viewport_h_px);
            return;
        }

        // Across-zoom scroll preservation: capture the user's current
        // logical reading position *before* we throw away the old
        // layout. We measure as "fraction into the current page";
        // this stays sensible across resize and zoom. If the user is
        // currently parked in an inter-page gap, `page_at` assigns
        // the gap to the page above, so `local_y` can exceed
        // `cur_page_h` by up to `gap_px`. Clamp to [0,1] so the
        // restored position never overshoots into the next page.
        let preserve = if self.last_layout_key.is_some() {
            let cur_page = self.layout.page_at(self.scroll_y_px);
            let cur_page_y = self.layout.page_y(cur_page);
            let cur_page_h = self.layout.page_h(cur_page).max(1) as f32;
            let frac = ((self.scroll_y_px - cur_page_y) as f32 / cur_page_h).clamp(0.0, 1.0);
            Some((cur_page, frac))
        } else {
            None
        };

        self.layout = PageLayout::build(&self.page_metrics, fit_width_px, layout_gap_px());

        if let Some((cur_page, frac)) = preserve {
            let new_page_y = self.layout.page_y(cur_page);
            let new_page_h = self.layout.page_h(cur_page) as f32;
            self.scroll_y_px = new_page_y + (frac * new_page_h) as i64;
        } else if let Some(page_idx) = self.pending_initial_page.take() {
            // Apply both the saved page AND the saved within-page
            // fraction so a reopen lands exactly where the user was
            // reading, not at the page boundary above.
            let page_y = self.layout.page_y(page_idx);
            let page_h = self.layout.page_h(page_idx) as f32;
            let frac = self.pending_initial_scroll_in_page;
            self.pending_initial_scroll_in_page = 0.0;
            self.scroll_y_px = page_y + (frac.clamp(0.0, 1.0) * page_h) as i64;
        }

        self.scroll_y_px = self.layout.clamp_scroll(self.scroll_y_px, viewport_h_px);
        self.last_layout_key = Some(key);
        self.page_cache.clear();
        self.overlay_cache.clear();
        self.highlights_baked_cache.clear();
        // Auxiliary state HashSets and the LRU vec must be reset
        // alongside the bitmap caches. Otherwise a page that was
        // Fast-quality at the OLD layout still sits in
        // `pages_at_fast_quality`; the next idle tick's
        // `upgrade_one_visible_to_sharp` re-renders it Sharp at the
        // NEW layout while the disk cache could have served it for
        // free. The LRU vec also accumulates stale page indices that
        // walk through `enforce_byte_budget`'s loop on every byte
        // pass until they self-clean.
        self.page_cache_lru.clear();
        self.pages_at_fast_quality.clear();
        // `pages_in_flight` keys on (page, fit_width_px, dark); after
        // a layout change every entry's `fit_width_px` is stale, and
        // the worker (when implemented) would still respond with the
        // OLD-width bitmap that the renderer now ignores. Cheaper to
        // drop the slot and let `request_prefetch` re-issue at the
        // new layout.
        self.pages_in_flight.clear();
        // A page can OOM at huge zoom but render fine after the user
        // zooms back out — don't keep it permanently blacklisted.
        self.failed_pages.clear();
        self.image_proto = None;
        self.last_compose_key = None;
        self.last_selection_range = None;
    }

    /// Drop cached page bitmaps (and their overlay derivatives) that
    /// are far from the visible window. Keeps a generous prefetch
    /// margin so steady scrolling rarely re-renders.
    pub fn evict_far_pages(&mut self, visible: std::ops::Range<usize>) {
        const MARGIN: usize = 3;
        let lo = visible.start.saturating_sub(MARGIN);
        let hi = visible.end.saturating_add(MARGIN);
        self.page_cache.retain(|&k, _| k >= lo && k < hi);
        self.overlay_cache.retain(|&k, _| k >= lo && k < hi);
        self.highlights_baked_cache
            .retain(|&k, _| k >= lo && k < hi);
        self.page_cache_lru.retain(|&k| k >= lo && k < hi);
        // Auxiliary state HashSets that gate render-quality logic.
        // Without pruning these alongside `page_cache`, a page can be
        // evicted from `page_cache` while still flagged as Fast or
        // in-flight — `upgrade_one_visible_to_sharp` then re-renders
        // a page no longer in cache, and `request_prefetch`'s
        // `pages_in_flight.contains` gate refuses to re-prefetch a
        // page whose render-worker request was lost mid-flight.
        self.pages_at_fast_quality
            .retain(|&k| k >= lo && k < hi);
        self.pages_in_flight
            .retain(|key| key.0 >= lo && key.0 < hi);
        // The text-layout cache holds char bbox + line index per page;
        // a few hundred KB per page in dense documents. Drop any entry
        // outside the visible window unless it's part of the active
        // selection — pin the FULL anchor..=head range so a multi-page
        // yank doesn't silently lose middle pages whose layout was
        // evicted while the user dragged across them.
        let pin_range = self.text_selection.map(|s| {
            let (a, b) = s.ordered();
            a.page..=b.page
        });
        self.text_cache.retain(|page| {
            (page >= lo && page < hi) || pin_range.as_ref().is_some_and(|r| r.contains(&page))
        });
    }

    /// Read the page's composed bitmap (highlights + search hits +
    /// optional selection band). Source ladder:
    ///   1. `overlay_cache` — populated when the active selection
    ///      touches this page. Includes saved highlights, search,
    ///      AND the selection band.
    ///   2. `highlights_baked_cache` — populated when the page has
    ///      saved highlights or search hits but no live selection.
    ///   3. `page_cache` (fallback) — the raw page bitmap when
    ///      neither overlay tier has anything to add. The kitty
    ///      path has its own fallback to page_cache (see
    ///      `draw_pages_kitty`); the canvas path goes through this
    ///      function and previously returned None for highlight-and-
    ///      search-free pages, leaving them painted as the canvas
    ///      background. Without this fallback a sixel/iterm/halfblock
    ///      session reading a doc with no highlights would render
    ///      every page blank — and `try_selection_only_repaint`
    ///      would leave the prior frame's stale selection band
    ///      ghosting on the canvas.
    pub fn composed_image(&self, page_idx: usize) -> Option<&RgbaImage> {
        if let Some((img, _)) = self.overlay_cache.get(&page_idx) {
            return Some(img);
        }
        if let Some((img, _)) = self.highlights_baked_cache.get(&page_idx) {
            return Some(img);
        }
        self.page_cache.get(&page_idx).and_then(|d| d.as_rgba8())
    }

    /// Mark a page as the most-recently-used. Called every time
    /// `ensure_image` reads or inserts a bitmap. Fast-paths the
    /// steady-scroll case where the page is already at MRU — skips
    /// the O(n) `retain` that would otherwise scan the whole window
    /// on every redraw.
    pub fn touch_page(&mut self, page: usize) {
        if self.page_cache_lru.last().copied() == Some(page) {
            return;
        }
        self.page_cache_lru.retain(|&p| p != page);
        self.page_cache_lru.push(page);
    }

    /// Evict the least-recently-used cached pages until the total
    /// byte cost of `page_cache` is below `budget`. Pages currently
    /// in `pinned` (visible right now) are never evicted, even if
    /// the budget can't be satisfied without them.
    pub fn enforce_byte_budget(&mut self, budget: usize, pinned: std::ops::Range<usize>) {
        // Compute the total byte cost ONCE, then maintain it
        // incrementally as we evict. Previously this re-summed every
        // cached page on every loop iteration — O(n²) per call.
        // Includes overlay_cache AND highlights_baked_cache: each is
        // a same-dimension copy of the source bitmap. Counting all
        // three in the initial total keeps the per-iteration sub
        // accounting honest — leaving highlights_baked_cache out
        // here used to under-count `total`, so the eviction loop
        // exited too early and the cache could overshoot the budget.
        let img_bytes = |img: &DynamicImage| (img.width() * img.height() * 4) as usize;
        let rgba_bytes = |img: &RgbaImage| (img.width() * img.height() * 4) as usize;
        let mut total: usize = self.page_cache.values().map(img_bytes).sum::<usize>()
            + self
                .overlay_cache
                .values()
                .map(|(img, _)| rgba_bytes(img))
                .sum::<usize>()
            + self
                .highlights_baked_cache
                .values()
                .map(|(img, _)| rgba_bytes(img))
                .sum::<usize>();
        if total <= budget {
            return;
        }
        // Build the eviction set in a single oldest-first pass through
        // the LRU, then drop the entries from page_cache_lru with one
        // retain. The previous loop did `retain(|&x| x != p)` per
        // evicted page — O(K·N) for K evictions and N entries; a long
        // run of evictions on a 700-page doc was visibly stuttery on
        // the first scroll past the budget. Single-pass is O(N + K).
        let mut to_evict: Vec<usize> = Vec::new();
        for &p in &self.page_cache_lru {
            if total <= budget {
                break;
            }
            if pinned.contains(&p) {
                continue;
            }
            let mut freed: usize = 0;
            if let Some(img) = self.page_cache.get(&p) {
                freed += img_bytes(img);
            }
            if let Some((img, _)) = self.overlay_cache.get(&p) {
                freed += rgba_bytes(img);
            }
            if let Some((img, _)) = self.highlights_baked_cache.get(&p) {
                freed += rgba_bytes(img);
            }
            total = total.saturating_sub(freed);
            to_evict.push(p);
        }
        if to_evict.is_empty() {
            // Everything left is pinned; budget is undersized for the
            // current viewport. Better to overshoot than refuse to
            // render.
            return;
        }
        for p in &to_evict {
            self.page_cache.remove(p);
            self.overlay_cache.remove(p);
            self.highlights_baked_cache.remove(p);
        }
        // Use a HashSet for O(1) membership rather than O(K) per
        // retained item.
        let evict_set: std::collections::HashSet<usize> = to_evict.into_iter().collect();
        self.page_cache_lru.retain(|p| !evict_set.contains(p));
        // Mirror the auxiliary-state pruning that `evict_far_pages`
        // does: a page evicted from `page_cache` here must also leave
        // `pages_at_fast_quality` (else the idle-upgrade loop tries to
        // re-render a no-longer-cached page) and `pages_in_flight`
        // (else `request_prefetch`'s contains-gate refuses to re-issue
        // a render request whose cache entry was just freed). text_cache
        // isn't byte-budget-pinned but the same staleness applies — drop
        // entries for pages whose bitmap was just evicted (selection-pin
        // pages are protected by the layout-visible budget caller anyway).
        self.pages_at_fast_quality
            .retain(|k| !evict_set.contains(k));
        self.pages_in_flight
            .retain(|key| !evict_set.contains(&key.0));
        self.text_cache.retain(|p| !evict_set.contains(&p));
    }

    /// Pages worth speculatively rendering ahead of the current
    /// viewport, in priority order (most useful first). Used by
    /// `ui::ensure_image` to fill in pages just outside the visible
    /// range, biased by `last_scroll_dir` so a downward scroll
    /// preloads pages below.
    pub fn prefetch_targets(&self, visible: std::ops::Range<usize>) -> Vec<usize> {
        const PREFETCH: usize = 2;
        let mut out: Vec<usize> = Vec::with_capacity(PREFETCH * 2);
        if self.last_scroll_dir >= 0 {
            for i in 0..PREFETCH {
                let p = visible.end + i;
                if p < self.page_count {
                    out.push(p);
                }
            }
            for i in 1..=PREFETCH {
                if let Some(p) = visible.start.checked_sub(i) {
                    out.push(p);
                }
            }
        } else {
            for i in 1..=PREFETCH {
                if let Some(p) = visible.start.checked_sub(i) {
                    out.push(p);
                }
            }
            for i in 0..PREFETCH {
                let p = visible.end + i;
                if p < self.page_count {
                    out.push(p);
                }
            }
        }
        out
    }

    /// Page that contains the viewport center — what the user is
    /// *reading*. Status line and session save use this so the
    /// number on screen matches the visual focus, not the scroll
    /// position. Use `leading_page()` for navigation.
    pub fn current_page(&self) -> usize {
        let center = self.scroll_y_px + (self.viewport_px.1 as i64 / 2);
        self.layout
            .page_at(center)
            .min(self.page_count.saturating_sub(1))
    }

    /// Page that contains the *top* of the viewport. This is what
    /// `j`/`k` advance from: a `j` press should always reveal new
    /// content, never skip a page just because a short page is
    /// already partially visible above the viewport center.
    pub fn leading_page(&self) -> usize {
        self.layout
            .page_at(self.scroll_y_px)
            .min(self.page_count.saturating_sub(1))
    }

    pub fn invalidate_compose(&mut self) {
        self.last_compose_key = None;
    }

    /// Get a slice of `viewport_w * 4` background-color bytes — one
    /// pre-baked row of (R, G, B, 255) tuples for `copy_from_slice`
    /// into target rows. Cached by (viewport_w, dark): a steady-scroll
    /// frame at unchanged dimensions reuses the same Vec instead of
    /// rebuilding it. Saves ~viewport_w*4 bytes of allocator churn per
    /// frame at 1080p (~7600 bytes), plus the iterator chain that
    /// builds it.
    pub fn bg_row(&mut self, viewport_w: u32) -> &[u8] {
        let key = (viewport_w, self.dark);
        if self.bg_row_key != Some(key) {
            let bg: [u8; 4] = if self.dark {
                [20, 20, 20, 255]
            } else {
                [240, 240, 240, 255]
            };
            let len = (viewport_w as usize) * 4;
            // Resize-then-fill chunks: one alloc (or grow), one
            // bounds-checked memset per pixel quad. Cheaper than the
            // earlier `viewport_w` × extend_from_slice(&bg) loop, which
            // walked the bounds check + amortised growth for each
            // 4-byte append.
            self.bg_row_buf.resize(len, 0);
            for chunk in self.bg_row_buf.chunks_exact_mut(4) {
                chunk.copy_from_slice(&bg);
            }
            self.bg_row_key = Some(key);
        }
        &self.bg_row_buf
    }

    /// True if the user is in a sustained input burst (autorepeat
    /// j/k, mouse-wheel spam, held-arrow, tap-tap reading). Used by
    /// the kitty draw path to defer cold-page transmits — each cold
    /// transmit ships hundreds of KB of base64 + a heavy pdfium render;
    /// burning that per keystroke at 5 Hz drives sustained 35 W+ on a
    /// laptop and the user reported 75-90 °C as a result.
    ///
    /// Two-condition check: most recent input was within
    /// `RAPID_SCROLL_THRESHOLD_MS` AND we've seen at least
    /// `RAPID_SCROLL_BURST_MIN` consecutive inputs in the window.
    /// The count guard prevents a single isolated keypress (count=1)
    /// from being flagged as a burst — that would defer the only
    /// page the user wanted to see.
    ///
    /// Threshold = 350 ms: covers normal reading-cadence tap-tap j
    /// (~4-5 Hz) plus mouse-wheel spam (50-200 ms) and held-key
    /// autorepeat (~30 ms). The earlier 120 ms ceiling missed normal
    /// reading and let the cold-render cost burn through every
    /// keystroke.
    pub fn is_rapid_scrolling(&self) -> bool {
        let recent = match self.last_input_at {
            Some(t) => t.elapsed().as_millis() < RAPID_SCROLL_THRESHOLD_MS,
            None => false,
        };
        recent && self.input_burst_count >= RAPID_SCROLL_BURST_MIN
    }

    /// Record that an input event just arrived. Increments the burst
    /// counter when consecutive inputs land within the rapid-scroll
    /// window; resets it otherwise so isolated keypresses don't get
    /// stuck in burst mode after a long pause.
    pub fn note_input(&mut self) {
        let now = std::time::Instant::now();
        let in_window = self
            .last_input_at
            .map(|t| (now - t).as_millis() < RAPID_SCROLL_THRESHOLD_MS)
            .unwrap_or(false);
        self.input_burst_count = if in_window {
            self.input_burst_count.saturating_add(1)
        } else {
            1
        };
        self.last_input_at = Some(now);
    }

    /// Force-clear the rapid-input-burst state.
    ///
    /// The settle-redraw branch in main::run_loop fires after the
    /// poll wait times out (no input for SETTLE_MS = 120 ms) and is
    /// supposed to be the catch-up draw that transmits any pages
    /// deferred during the burst. But `is_rapid_scrolling` returns
    /// true until `last_input_at` is RAPID_SCROLL_THRESHOLD_MS old —
    /// 130 ms longer than SETTLE_MS. Without an explicit reset, the
    /// catch-up draw still sees `is_rapid_scrolling = true` and
    /// re-defers the cold pages, leaving the current page blank
    /// until the next user input or 130 ms later. Resetting the
    /// burst count here costs nothing on the next real scroll: the
    /// new event re-arms the counter from 1.
    pub fn clear_input_burst(&mut self) {
        self.input_burst_count = 0;
    }

    /// Gate for scroll-key handlers (`j`, `k`, Space, `b`, Ctrl-d/u,
    /// `]]`, `[[`). Returns `true` if the scroll should proceed and
    /// records the time; returns `false` if the previous accepted
    /// scroll was within `SCROLL_THROTTLE_MS` ago. Held-key autorepeat
    /// at 30 Hz would otherwise flip 30 PDF pages/sec; the throttle
    /// caps that at ~6.7 Hz, which lands at a brisk reading cadence
    /// without feeling laggy on intentional taps further than 150 ms
    /// apart.
    pub fn note_scroll_attempt(&mut self) -> bool {
        let now = std::time::Instant::now();
        let allow = self
            .last_scroll_applied_at
            .is_none_or(|t| (now - t).as_millis() >= SCROLL_THROTTLE_MS);
        if allow {
            self.last_scroll_applied_at = Some(now);
        }
        allow
    }

    /// Derive the active selection's per-page fingerprint for the
    /// page-overlay cache key. Returns 0 when the selection is empty
    /// or doesn't touch this page so a non-baking page hits the cache
    /// regardless of selection state on other pages.
    pub fn selection_signature_for_page(&self, page_idx: usize) -> u64 {
        let Some(sel) = self.text_selection else {
            return 0;
        };
        let (lo, hi) = sel.ordered();
        if page_idx < lo.page || page_idx > hi.page {
            return 0;
        }
        // FNV-style mix; cheap and good enough for cache invalidation.
        let mut h: u64 = 0xcbf29ce484222325;
        let mut mix = |v: u64| {
            h ^= v;
            h = h.wrapping_mul(0x100000001b3);
        };
        mix(lo.page as u64);
        mix(lo.idx as u64);
        mix(hi.page as u64);
        mix(hi.idx as u64);
        mix(self.selection_color_idx as u64);
        mix(match sel.mode {
            crate::textlayout::SelMode::Charwise => 1,
            crate::textlayout::SelMode::Linewise => 2,
            crate::textlayout::SelMode::Blockwise => 3,
        });
        mix(if self.selection_placement { 1 } else { 0 });
        // Distinguish "no selection" (0) from any real selection by
        // forcing the low bit on. Real signatures are always odd.
        h | 1
    }

    /// Process-global selection fingerprint for the compose tier.
    /// 0 if there's no active selection.
    pub fn selection_signature_global(&self) -> u64 {
        let Some(sel) = self.text_selection else {
            return 0;
        };
        let (lo, hi) = sel.ordered();
        let mut h: u64 = 0xcbf29ce484222325;
        let mut mix = |v: u64| {
            h ^= v;
            h = h.wrapping_mul(0x100000001b3);
        };
        mix(lo.page as u64);
        mix(lo.idx as u64);
        mix(hi.page as u64);
        mix(hi.idx as u64);
        mix(self.selection_color_idx as u64);
        mix(match sel.mode {
            crate::textlayout::SelMode::Charwise => 1,
            crate::textlayout::SelMode::Linewise => 2,
            crate::textlayout::SelMode::Blockwise => 3,
        });
        mix(if self.selection_placement { 1 } else { 0 });
        h | 1
    }

    pub fn goto_page(&mut self, page: usize) {
        // Record the source on the jumplist for `<C-o>` round-trip,
        // but only on a "big" jump — sequential j/k stepping would
        // otherwise drown the jumplist in adjacent pages. ≥ 2-page
        // delta matches vim's heuristic for what counts as a jump.
        let from = self.current_page();
        let p = page.min(self.page_count.saturating_sub(1));
        if from.abs_diff(p) >= 2 {
            self.push_jump(from);
        }
        self.goto_page_no_record(p);
    }

    /// Jump to the next (`dir = +1`) or previous (`dir = -1`) outline
    /// entry by page. Skips entries with no resolved page. No-op if
    /// the document has no outline or if we're already at the
    /// boundary in the requested direction.
    ///
    /// Vim convention: `]]` next, `[[` prev. The first jump from a
    /// page that's mid-section lands on the next/previous *boundary*
    /// (so `]]` from the middle of section 3 goes to section 4, and
    /// `[[` goes to the start of section 3).
    /// Jump to the document's references / bibliography section.
    /// Heuristic: scan the outline for an entry titled (case-
    /// insensitively) "References", "Bibliography", or "Works Cited".
    /// First match wins. No-op with status if no such entry exists.
    pub fn jump_to_references(&mut self) {
        let target = find_references_page(&self.outline);
        match target {
            Some(p) => {
                self.goto_page(p);
                self.status = format!("→ references (page {})", p + 1);
            }
            None => {
                self.status = "no References / Bibliography section found in outline".into();
            }
        }
    }

    pub fn jump_section(&mut self, dir: i32) {
        if self.outline.is_empty() {
            self.status = "no outline in this document".into();
            return;
        }
        if self.outline_pages_sorted.is_empty() {
            self.status = "outline has no resolved pages".into();
            return;
        }
        match next_section_target(&self.outline_pages_sorted, self.current_page(), dir) {
            Some(p) => {
                self.goto_page(p);
                self.status = format!("→ page {}", p + 1);
            }
            None => {
                self.status = if dir > 0 {
                    "no next section".into()
                } else {
                    "no prev section".into()
                };
            }
        }
    }

    /// Same as `goto_page` but does NOT touch the jumplist. Used by
    /// `<C-o>`/`<C-i>` themselves so walking the list doesn't keep
    /// re-pushing the destinations as new jumps.
    pub fn goto_page_no_record(&mut self, page: usize) {
        let p = page.min(self.page_count.saturating_sub(1));
        self.scroll_y_px = self
            .layout
            .clamp_scroll(self.layout.page_y(p), self.viewport_px.1);
        self.invalidate_compose();
    }

    pub fn next_page(&mut self, count: usize) {
        let target = self.leading_page().saturating_add(count.max(1));
        self.goto_page(target);
    }
    pub fn prev_page(&mut self, count: usize) {
        let target = self.leading_page().saturating_sub(count.max(1));
        self.goto_page(target);
    }
    pub fn first_page(&mut self) {
        let from = self.current_page();
        if from >= 2 {
            self.push_jump(from);
        }
        self.scroll_y_px = 0;
        self.invalidate_compose();
    }
    /// `G` lands on the *top* of the last page, not the doc bottom.
    /// Matches `:N` and counted `NG` semantics so all "go to page p"
    /// paths agree on where p starts.
    pub fn last_page(&mut self) {
        let from = self.current_page();
        let last = self.page_count.saturating_sub(1);
        if from.abs_diff(last) >= 2 {
            self.push_jump(from);
        }
        self.scroll_y_px = self
            .layout
            .clamp_scroll(self.layout.page_y(last), self.viewport_px.1);
        self.invalidate_compose();
    }

    pub fn toggle_dark(&mut self) {
        self.dark = !self.dark;
        // dark flag is part of LayoutKey → cache will rebuild next
        // frame. Force compose invalidation to repaint immediately.
        self.invalidate_compose();
    }

    /// Multiply zoom by `factor`, clamp to a sensible range. Layout
    /// is rebuilt on the next frame because fit_width depends on it;
    /// scroll position auto-adjusts to keep the same logical reading
    /// position (see `ensure_layout`).
    pub fn zoom_by(&mut self, factor: f32) {
        // Reject NaN/Inf inputs — `f32::clamp(NaN, ..)` returns NaN
        // (not the bound), so a NaN factor would silently poison
        // `self.zoom` and propagate through every fit_width and
        // src/dst pixel calculation forever after.
        if !factor.is_finite() {
            return;
        }
        let new = (self.zoom * factor).clamp(0.25, 8.0);
        if !new.is_finite() {
            return;
        }
        if (new - self.zoom).abs() < f32::EPSILON {
            return;
        }
        self.zoom = new;
        if (self.zoom - 1.0).abs() < 0.001 {
            self.scroll_x = 0.0;
        }
        self.invalidate_compose();
    }

    pub fn zoom_reset(&mut self) {
        self.zoom = 1.0;
        self.scroll_x = 0.0;
        self.invalidate_compose();
    }

    /// Scroll vertically by a number of pixels (positive = down).
    /// Clamped to document bounds. Saturating add guards against an
    /// `i64::MIN`/`i64::MAX` `dy_px` from a user-supplied count.
    /// Records the sign so `prefetch_targets` knows which direction
    /// to bias the speculative renders.
    pub fn scroll_by_px(&mut self, dy_px: i64) {
        let new = self
            .layout
            .clamp_scroll(self.scroll_y_px.saturating_add(dy_px), self.viewport_px.1);
        if new != self.scroll_y_px {
            self.last_scroll_dir = (new - self.scroll_y_px).signum() as i8;
            self.scroll_y_px = new;
            self.invalidate_compose();
        }
    }

    /// Vertical scroll by a fraction of the viewport height
    /// (e.g. 0.1 = 10% of a screen).
    pub fn scroll_by_screens(&mut self, dy_screens: f32) {
        let viewport_h = self.viewport_px.1 as i64;
        self.scroll_by_px((viewport_h as f32 * dy_screens).round() as i64);
    }

    /// Horizontal scroll, only meaningful when fit_width > viewport_w
    /// (i.e. zoomed in past 100% width).
    pub fn scroll_x_by(&mut self, dx_screens: f32) {
        let (vw, _) = self.viewport_px;
        let overflow = self.layout.fit_width_px.saturating_sub(vw) as f32;
        if overflow <= 0.0 || vw == 0 {
            return;
        }
        let dx = (vw as f32 * dx_screens) / overflow;
        let new = (self.scroll_x + dx).clamp(0.0, 1.0);
        if (new - self.scroll_x).abs() > f32::EPSILON {
            self.scroll_x = new;
            self.invalidate_compose();
        }
    }

    /// Drop any half-typed Normal-mode chord state. Called on every
    /// mode entry so that e.g. `5g:` (numeric prefix → Command) doesn't
    /// leave `5g` sitting in `pending` to fire later, and `m:` (mark-
    /// set → Command) doesn't silently consume the next post-Esc
    /// keystroke as the mark name. Also exits any active link-hint or
    /// TOC-filter modal — those are independent navigation modes that
    /// should not survive a transition into Visual / mouse drag / etc.
    pub fn clear_chord_state(&mut self) {
        self.pending.clear();
        self.awaiting_mark_set = false;
        self.awaiting_mark_jump = false;
        self.awaiting_highlight_delete_confirm = false;
        self.link_hint_mode = false;
        self.hint_filter.clear();
        self.toc_filter_editing = false;
    }

    pub fn enter_visual(&mut self) {
        // Place the caret at the first char that's *actually visible*
        // in the viewport — not at idx 0 (top of page). If the user
        // is reading mid-page and the anchor lands above the viewport,
        // every selection rect they grow with hjkl paints in cells
        // above the visible area, so the live overlay is invisible
        // until they scroll up — was the "highlights only show after
        // y" bug. Falls back to idx 0 if nothing on the page is in
        // view (which only happens when the page header is offscreen
        // by more than its own height — an unusual layout state).
        let page = self.current_page();
        let metrics = match self.page_metrics.get(page).copied() {
            Some(m) => m,
            None => return,
        };
        let pt = match self.text_cache.get_or_load(&self.document, page, &metrics) {
            Ok(pt) => pt,
            Err(e) => {
                // pdfium's Display impl can reflect bytes from the
                // document — sanitise before letting them hit the
                // status line / wire.
                let safe = crate::term_safe::safe_for_stderr(&format!("{e:#}"));
                self.status = format!("page {}: cannot read text ({safe})", page + 1);
                return;
            }
        };
        if pt.chars.is_empty() {
            self.status = format!("page {}: no selectable text", page + 1);
            return;
        }
        // Compute viewport-top-in-page-norm BEFORE calling the pure
        // helper so we don't end up needing both a mutable borrow on
        // self.text_cache (via pt) and an immutable borrow on self.
        let page_top_doc = self.layout.page_y(page);
        let page_h_px = self.layout.page_h(page).max(1) as f32;
        let viewport_top_norm = ((self.scroll_y_px - page_top_doc) as f32) / page_h_px;
        let idx = first_visible_char_idx_pure(viewport_top_norm, pt).unwrap_or(0);
        let caret = Caret { page, idx };
        self.text_selection = Some(TextSelection::new(caret));
        self.mode = Mode::Visual;
        // Land in selection mode (vim semantics): the next hjkl/wbe
        // immediately extends the band from the anchor. Earlier this
        // started in `placement` mode where motions kept the band
        // collapsed (anchor followed head) and a second `v` was
        // required to "lock" the anchor — non-vim and surprising.
        // `v` while in Visual still toggles back to placement for
        // users who want to reposition the caret first.
        self.selection_placement = false;
        self.clear_chord_state();
        self.status = "VISUAL · selecting — y save · Y copy · c color · v relocate · Esc".into();
    }

    /// Toggle between placement (caret moves freely, anchor follows
    /// head) and selection (anchor locked, motions grow the band).
    /// `v` in Visual mode runs this; entering Visual via `v` from
    /// Normal starts in placement so the user can position before
    /// committing to a selection.
    pub fn toggle_selection_placement(&mut self) {
        if self.mode != Mode::Visual {
            return;
        }
        self.selection_placement = !self.selection_placement;
        self.status = if self.selection_placement {
            "VISUAL · placement — hjkl moves caret · v starts selection · Esc cancels".into()
        } else {
            "VISUAL · selecting — y save · Y copy · c color · v relocate anchor · Esc".into()
        };
        self.invalidate_compose();
    }

    /// If we're in placement mode, sync the anchor to wherever the
    /// head just landed so motions move both together (i.e. no band
    /// growth). Called after every caret-motion in `keys::visual_keys`.
    pub fn sync_anchor_to_head_if_placing(&mut self) {
        if !self.selection_placement {
            return;
        }
        if let Some(sel) = self.text_selection.as_mut() {
            sel.anchor = sel.head;
        }
    }

    /// If the head caret is outside the current viewport, scroll just
    /// enough to bring it back into view. Called after every caret-
    /// motion method so growing the selection past the viewport edge
    /// drags the page along — otherwise the user mashes `j` and the
    /// selection keeps growing offscreen.
    pub fn scroll_to_head_if_offscreen(&mut self) {
        let Some(sel) = self.text_selection else {
            return;
        };
        let Some(pt) = self.text_cache.get(sel.head.page) else {
            return;
        };
        let Some(c) = pt.chars.get(sel.head.idx) else {
            return;
        };
        let page_top = self.layout.page_y(sel.head.page);
        let page_h = self.layout.page_h(sel.head.page).max(1) as i64;
        let head_top_doc = page_top + (c.bbox.y * page_h as f32) as i64;
        let head_bot_doc = page_top + ((c.bbox.y + c.bbox.h) * page_h as f32) as i64;
        let vh = self.viewport_px.1 as i64;
        if vh <= 0 {
            return;
        }
        // Margin of one cell-row so the caret never sits exactly on
        // the edge — gives a sliver of context above/below.
        let margin = self.cell_size_px.1 as i64;
        let viewport_top = self.scroll_y_px;
        let viewport_bot = viewport_top + vh;
        let new_scroll = if head_top_doc < viewport_top + margin {
            head_top_doc - margin
        } else if head_bot_doc > viewport_bot - margin {
            head_bot_doc - vh + margin
        } else {
            return; // already in view
        };
        let clamped = self.layout.clamp_scroll(new_scroll, vh as u32);
        if clamped != self.scroll_y_px {
            self.scroll_y_px = clamped;
            self.invalidate_compose();
        }
    }

    pub fn exit_visual(&mut self) {
        self.text_selection = None;
        self.mouse_dragging = false;
        self.selection_placement = false;
        self.mode = Mode::Normal;
        self.status.clear();
        // Clear the previous-frame selection-range hint. Without this,
        // `try_selection_only_repaint` after exit-visual can attempt
        // a fast-path repaint of a now-nonexistent selection's prior
        // range, doing pointless work (and worse, the math on
        // None-selection assumes the field is None too).
        self.last_selection_range = None;
        // Drop leftover chord state (e.g. partial `g` from `gg`,
        // dangling awaiting_mark_*) so the next Normal-mode keystroke
        // isn't misinterpreted.
        self.clear_chord_state();
    }

    /// Yank the active Visual-mode selection: extract its text,
    /// copy to the clipboard, and (if `save`) persist a highlight on
    /// the selection's bound page. Returns to Normal mode.
    ///
    /// `save = true` is the `y` keybinding (highlight + copy);
    /// `save = false` is `Y` (copy text without leaving a yellow
    /// box). Either way, an empty extraction (image-only region) is
    /// surfaced in the status line rather than silently swallowed —
    /// users want to know if their quote didn't actually land.
    pub fn yank_selection(&mut self, save: bool) {
        // Defensive: a fast mouse-drag may have skipped intermediate
        // pages (see `mouse_drag_to`'s ensure call). Load any missing
        // page text up-front so the yank loop below doesn't quietly
        // omit pages from the saved highlight + clipboard text.
        self.ensure_selection_text_loaded();
        let Some(sel) = self.text_selection.take() else {
            self.mode = Mode::Normal;
            return;
        };
        let color = HIGHLIGHT_COLORS[self.selection_color_idx % HIGHLIGHT_COLORS.len()];

        // Walk pages in document order, asking each page's text
        // layout for the substring of its char range that's inside
        // the selection. Concatenate with `\n\n` between pages.
        let (lo, hi) = sel.ordered();
        let mut combined = String::new();
        let page_span = hi.page.saturating_sub(lo.page) + 1;
        let mut per_page_rects: Vec<(usize, Vec<Rect01>)> = if save {
            Vec::with_capacity(page_span)
        } else {
            Vec::new()
        };
        for page_idx in lo.page..=hi.page {
            let Some(pt) = self.text_cache.get(page_idx) else {
                continue;
            };
            let mut start = if page_idx == lo.page { lo.idx } else { 0 };
            let mut end = if page_idx == hi.page {
                hi.idx
            } else {
                pt.chars.len().saturating_sub(1)
            };
            // Linewise mode: widen `start`/`end` to whole lines —
            // mirroring `bake_selection_into_page`'s widen logic
            // (ui.rs near `pt.line_of(start)`). Without this, the
            // user sees a full-line highlight band on screen but
            // the saved rect and the clipboard text only cover the
            // original char range — confusing, and a real-today
            // bug in `V`-mode yanks.
            if matches!(sel.mode, SelMode::Linewise) {
                if let Some(line) = pt.line_of(start) {
                    if let Some(s) = pt.line_start(line) {
                        start = s;
                    }
                }
                if let Some(line) = pt.line_of(end) {
                    if let Some(e) = pt.line_end(line) {
                        end = e;
                    }
                }
            }
            // Extract first; only emit a page separator if this page
            // actually contributed text. Otherwise an image-only page
            // mid-selection would leave dangling `\n\n\n\n` runs, and
            // an evicted page would silently fuse its neighbours.
            let s = pt.extract(start, end);
            if !s.is_empty() {
                if !combined.is_empty() {
                    combined.push_str("\n\n");
                }
                combined.push_str(&s);
            }
            if save {
                per_page_rects.push((page_idx, pt.range_to_rects(start, end)));
            }
        }
        let text = combined.trim();

        let copy_outcome = if !text.is_empty() {
            Some(crate::clipboard::copy(text))
        } else {
            None
        };

        if save {
            // All rects from a single yank share one group_id so a
            // later `x` can wipe the whole multi-line highlight in
            // one keystroke instead of N (the user used to see only
            // the last band disappear). Counter is monotonic per
            // process; persisted to AnnotMeta so identity survives
            // save+reopen.
            let group_id = self.alloc_highlight_group_id();
            let mut added_any = false;
            for (page_idx, rects) in per_page_rects {
                for r in rects {
                    if r.w < 1e-4 || r.h < 1e-4 {
                        continue;
                    }
                    self.highlights.add(Highlight {
                        page: page_idx,
                        x: r.x,
                        y: r.y,
                        w: r.w,
                        h: r.h,
                        color: color.hex.into(),
                        note: None,
                        group_id: Some(group_id),
                    });
                    added_any = true;
                }
            }
            // Bump the revision and invalidate compose only if we
            // actually added something. If every page in the selection
            // had no rects (text not cached) or every rect was below
            // the 1e-4 floor, bumping busts every overlay/baked entry
            // for nothing — major churn on a heavily-highlighted doc.
            if added_any {
                self.highlight_revision += 1;
                self.invalidate_compose();
                // Auto-persist so a panic / SIGKILL / power loss mid-
                // session bounds the worst-case loss to one yank instead
                // of "every highlight made since startup". Best-effort:
                // a write failure is surfaced via status but doesn't
                // abort the user's flow. ~50ms per call (filtered save
                // walks only touched pages) — acceptable per-yank cost
                // for the safety guarantee.
                if let Err(e) = self.persist_highlights() {
                    self.status = format!(
                        "highlight saved in memory; auto-persist failed: {}",
                        crate::term_safe::safe_for_stderr(&format!("{e:#}"))
                    );
                }
            }
        }

        // Distinguish "actually-copied" from "tried-and-both-paths-
        // failed". The CopyOutcome counts bytes against the payload
        // length whether or not either the OSC 52 path or the native
        // binary actually succeeded. Without this check, a session
        // that cannot reach `/dev/tty` AND has no clipboard binary
        // (rare but real for some headless containers) would still
        // show "copied N bytes" even though pasting later yields
        // nothing.
        let any_clip_path = copy_outcome
            .as_ref()
            .map(|o| o.osc52_written || o.native_written)
            .unwrap_or(false);
        self.status = match (save, copy_outcome) {
            (true, Some(o)) if !any_clip_path => {
                format!(
                    "highlight saved (clipboard unavailable; {} bytes)",
                    o.bytes
                )
            }
            (true, Some(o)) if o.truncated => {
                format!("highlight saved + copied {} bytes (truncated)", o.bytes)
            }
            (true, Some(o)) => format!("highlight saved + copied {} bytes", o.bytes),
            (true, None) => "highlight saved (no extractable text)".into(),
            (false, Some(o)) if !any_clip_path => {
                format!("clipboard unavailable ({} bytes not copied)", o.bytes)
            }
            (false, Some(o)) if o.truncated => {
                format!("copied {} bytes (truncated)", o.bytes)
            }
            (false, Some(o)) => format!("copied {} bytes", o.bytes),
            (false, None) => "no text in selection".into(),
        };

        self.mouse_dragging = false;
        self.mode = Mode::Normal;
    }

    /// Backwards-compatible name for `y` (save + copy). Visual-mode
    /// keybinding `y` and the search-helper module both call this.
    pub fn save_selection(&mut self) {
        self.yank_selection(true);
    }

    /// Run a full-document text search and scroll to the first hit.
    /// An empty query re-runs `last_query` (vim-style); an empty
    /// query with no `last_query` clears any active results.
    pub fn run_search(&mut self, query: &str) {
        let query = query.trim();
        let query: String = if query.is_empty() {
            match &self.last_query {
                Some(q) => q.clone(),
                None => {
                    self.search = None;
                    self.status.clear();
                    self.invalidate_compose();
                    return;
                }
            }
        } else {
            query.to_string()
        };

        match crate::search::run_search(
            &self.document,
            &self.page_metrics,
            &query,
            false,
            Some(&self.doc_index),
        ) {
            Ok(results) => {
                if results.hits.is_empty() {
                    self.status = format!("no matches for '{}'", query);
                    self.search = None;
                } else {
                    let pct = (self.doc_index.fraction_complete() * 100.0).round() as u32;
                    let mut suffix = String::new();
                    if results.truncated {
                        // Surface the cap so the user knows the count
                        // is a lower bound and "n of N" navigation
                        // doesn't span the full doc — narrowing the
                        // query will reveal more.
                        suffix.push_str(" (truncated; narrow query for more)");
                    }
                    if !self.doc_index.is_complete() {
                        suffix
                            .push_str(&format!(" (index {pct}% — search may improve as it fills)"));
                    }
                    self.status = format!(
                        "{}/{} matches for '{}'{}",
                        1,
                        results.hits.len(),
                        query,
                        suffix
                    );
                    self.search = Some(results);
                    self.last_query = Some(query);
                    self.scroll_to_current_hit();
                }
                self.invalidate_compose();
            }
            Err(e) => {
                let safe = crate::term_safe::safe_for_stderr(&format!("{e:#}"));
                self.status = format!("search error: {safe}");
                self.invalidate_compose();
            }
        }
    }

    /// Enter link-hint mode: enumerate every link on the currently
    /// visible pages, assign hint chars, and flip `link_hint_mode`.
    /// No-op if no links exist on screen (status line tells the user).
    #[allow(clippy::useless_conversion)] // ordered_float helper for stable sort
    pub fn enter_link_hint_mode(&mut self) {
        let viewport_h = self.viewport_px.1;
        if viewport_h == 0 {
            return;
        }
        self.clear_chord_state();
        let visible: Vec<usize> = self
            .layout
            .visible_pages(self.scroll_y_px, viewport_h)
            .collect();
        if visible.is_empty() {
            self.status = "no links: no pages in viewport".into();
            return;
        }

        // Collect raw links per visible page.
        let mut entries: Vec<HintEntry> = Vec::new();
        let pages = self.document.pages();
        for &page_idx in &visible {
            let Ok(page) = pages.get(page_idx as i32) else {
                continue;
            };
            let Some(metrics) = self.page_metrics.get(page_idx) else {
                continue;
            };
            for link in crate::links::enumerate(&page, metrics) {
                entries.push(HintEntry {
                    page_idx,
                    rect: link.rect,
                    action: link.action,
                    label: String::new(), // assigned below
                });
            }
        }
        if entries.is_empty() {
            self.status = "no links on visible pages".into();
            return;
        }

        // Assign hints in screen-reading order (page index, then top-
        // to-bottom, then left-to-right within a page) so the hint
        // labels feel predictable.
        entries.sort_by(|a, b| {
            (a.page_idx, ordered_float(a.rect.y), ordered_float(a.rect.x)).cmp(&(
                b.page_idx,
                ordered_float(b.rect.y),
                ordered_float(b.rect.x),
            ))
        });
        let labels = crate::links::gen_hints(entries.len());
        for (e, l) in entries.iter_mut().zip(labels.into_iter()) {
            e.label = l;
        }

        self.link_hints = entries;
        self.hint_filter.clear();
        self.link_hint_mode = true;
        self.status = format!("link mode: {} hints (Esc to cancel)", self.link_hints.len());
        self.invalidate_compose();
    }

    /// Append `c` to the hint filter; returns the action to dispatch
    /// if a unique hint matched, `None` if still ambiguous.
    /// Side-effect: on no-match or full-match, exits hint mode.
    pub fn hint_keystroke(&mut self, c: char) -> Option<LinkAction> {
        if !self.link_hint_mode {
            return None;
        }
        self.hint_filter.push(c);
        let filter = self.hint_filter.clone();
        let matches: Vec<&HintEntry> = self
            .link_hints
            .iter()
            .filter(|e| e.label.starts_with(&filter))
            .collect();
        match matches.len() {
            0 => {
                self.exit_link_hint_mode();
                self.status = format!("no hint matches `{filter}`");
                None
            }
            1 => {
                let action = matches[0].action.clone();
                self.exit_link_hint_mode();
                Some(action)
            }
            _ => {
                // Still ambiguous; redraw with narrowed hints.
                self.invalidate_compose();
                None
            }
        }
    }

    pub fn exit_link_hint_mode(&mut self) {
        self.link_hint_mode = false;
        self.link_hints.clear();
        self.hint_filter.clear();
        self.invalidate_compose();
    }

    /// Dispatch a chosen link-hint action.
    pub fn follow_link_action(&mut self, action: LinkAction) {
        match action {
            LinkAction::GoToPage(page_idx) => {
                let p = page_idx.min(self.page_count.saturating_sub(1));
                self.goto_page(p);
                self.status = format!("→ page {}", p + 1);
            }
            LinkAction::Url(url) => {
                // PDF link annotations are attacker-controlled. Two
                // hardenings before handing the URL to xdg-open:
                //
                //   1. Scheme allow-list. file://, javascript:, ssh://,
                //      tel:, etc. all dispatch through xdg-open's MIME
                //      handlers — file:// can leak/exfiltrate the
                //      user's filesystem, javascript: gets handed to
                //      a browser as code. Only http(s)/mailto are
                //      reasonable for a PDF reading workflow.
                //   2. `--` argv separator. xdg-open treats argv
                //      starting with `-` as flags ("--version",
                //      "--help"), so a malicious link target like
                //      "--version" would do something unexpected
                //      instead of erroring. The `--` ensures the URL
                //      is interpreted as a positional arg.
                // The URL is attacker-controlled (PDF link annotation
                // payload). ratatui filters C0/C1 escapes at render
                // time, but not bidi marks, line/paragraph
                // separators, or isolate controls — those can flip
                // the rendering of the rest of the status line. Run
                // the URL through `safe_for_stderr` everywhere it's
                // composed into a status string.
                let url_safe =
                    crate::term_safe::safe_for_stderr(&truncate_for_status(&url, 256));
                if !is_safe_external_url(&url) {
                    self.status = format!(
                        "blocked link with unsafe scheme: {}",
                        truncate_for_status(&url_safe, 80)
                    );
                } else {
                    let r = std::process::Command::new("xdg-open")
                        .arg("--")
                        .arg(&url)
                        .spawn();
                    match r {
                        Ok(_) => self.status = format!("opened: {url_safe}"),
                        Err(e) => {
                            let safe = crate::term_safe::safe_for_stderr(&format!("{e}"));
                            self.status = format!("xdg-open failed for {url_safe}: {safe}");
                        }
                    }
                }
            }
            LinkAction::Other => {
                self.status = "unsupported link type".into();
            }
        }
        self.invalidate_compose();
    }

    /// Move to the next/previous search hit, wrapping at the ends.
    /// `dir` is +1 or -1.
    pub fn advance_search(&mut self, dir: i32) {
        let Some(s) = self.search.as_mut() else {
            self.status = "no active search (try /pattern)".into();
            self.invalidate_compose();
            return;
        };
        if s.hits.is_empty() {
            self.invalidate_compose();
            return;
        }
        s.advance(dir);
        let cur = s.current + 1;
        let total = s.hits.len();
        let q = s.query.clone();
        self.status = format!("{}/{} matches for '{}'", cur, total, q);
        self.scroll_to_current_hit();
        self.invalidate_compose();
    }

    /// Drop the active search results and overlays. Wired to
    /// `:nohl` and to a fresh `/` with empty buffer + no last query.
    pub fn clear_search(&mut self) {
        if self.search.is_some() {
            self.search = None;
            self.status.clear();
            self.invalidate_compose();
        }
    }

    /// Scroll so the currently-focused search hit is visible. Lands
    /// the hit roughly a third of the way down the viewport — that
    /// keeps a few lines of context above and matches vim's `zz`-ish
    /// reading position.
    fn scroll_to_current_hit(&mut self) {
        let Some(s) = self.search.as_ref() else {
            return;
        };
        let Some(hit) = s.current_hit() else {
            return;
        };
        let page_y = self.layout.page_y(hit.page);
        // `.max(1)` guards against a zero-height page (DRM-stripped or
        // pdfium parse error returning empty rect). Without it
        // `hit_y_in_page = 0` and the scroll target is `page_y - vh/3`
        // — landing the user a third of a viewport above the broken
        // page, on the page above, which is confusing. Other call
        // sites (`scroll_to_head_if_offscreen`, `first_visible_char_idx`,
        // `ensure_layout`, `persist_session`) already use this pattern.
        let page_h = self.layout.page_h(hit.page).max(1) as i64;
        let hit_y_in_page = (hit.rect.y * page_h as f32) as i64;
        let target = page_y + hit_y_in_page - (self.viewport_px.1 as i64 / 3);
        self.scroll_y_px = self.layout.clamp_scroll(target.max(0), self.viewport_px.1);
    }

    /// Open the TOC panel. No-op (with a status message) if the
    /// document has no outline — a popping-up empty popup is
    /// confusing.
    pub fn toggle_toc(&mut self) {
        if self.show_toc {
            self.show_toc = false;
            self.toc_filter_editing = false;
            self.invalidate_compose();
            return;
        }
        if self.outline.is_empty() {
            self.status = "this document has no outline".into();
            self.invalidate_compose();
            return;
        }
        self.show_toc = true;
        self.toc_cursor = 0;
        self.toc_filter.clear();
        self.toc_filter_editing = false;
        self.invalidate_compose();
    }

    /// Indices of `outline` entries matching the current filter, in
    /// outline order. Memoised against `toc_filter` so opening a TOC
    /// with a stable filter doesn't re-lowercase the query and
    /// re-walk the outline 60 times a second from `draw_toc`. Empties
    /// when the filter changes via `toc_filter_*`. For small outlines
    /// the recompute is cheap; for long technical books with deep
    /// outlines it was visible CPU.
    pub fn toc_filtered_indices(&mut self) -> Vec<usize> {
        let needs_recompute = match &self.toc_filtered_cache {
            Some((cached_filter, _)) => cached_filter != &self.toc_filter,
            None => true,
        };
        if needs_recompute {
            let v = outline::fuzzy_filter(&self.outline, &self.toc_filter);
            self.toc_filtered_cache = Some((self.toc_filter.clone(), v));
        }
        // The cache owns the canonical Vec; the per-call clone is
        // ~30 × 8 bytes for a typical outline. The expensive part
        // (lowercasing the query, walking every entry doing
        // subsequence-match) is what the cache eliminates.
        self.toc_filtered_cache.as_ref().unwrap().1.clone()
    }

    pub fn toc_move(&mut self, delta: i32) {
        let cursor = self.toc_cursor;
        let filtered = self.toc_filtered_indices();
        if filtered.is_empty() {
            return;
        }
        let n = filtered.len() as i32;
        let new = ((cursor as i32) + delta).clamp(0, n - 1);
        self.toc_cursor = new as usize;
        self.invalidate_compose();
    }

    pub fn toc_jump_to_top(&mut self) {
        self.toc_cursor = 0;
        self.invalidate_compose();
    }

    pub fn toc_jump_to_bottom(&mut self) {
        let filtered = self.toc_filtered_indices();
        if let Some(last) = filtered.len().checked_sub(1) {
            self.toc_cursor = last;
        }
        self.invalidate_compose();
    }

    /// Activate the highlighted TOC entry: jump to its page (if
    /// resolvable) and close the panel.
    pub fn toc_activate(&mut self) {
        let cursor = self.toc_cursor;
        let filtered = self.toc_filtered_indices();
        let Some(&entry_idx) = filtered.get(cursor) else {
            return;
        };
        let entry = &self.outline[entry_idx];
        match entry.page {
            Some(p) => {
                self.show_toc = false;
                self.toc_filter_editing = false;
                self.goto_page(p);
            }
            None => {
                self.status = "outline entry has no page target".into();
                self.invalidate_compose();
            }
        }
    }

    pub fn toc_filter_push(&mut self, c: char) {
        if self.toc_filter_editing {
            self.toc_filter.push(c);
            self.toc_cursor = 0;
            self.invalidate_compose();
        }
    }

    pub fn toc_filter_pop(&mut self) {
        if self.toc_filter_editing {
            self.toc_filter.pop();
            self.toc_cursor = 0;
            self.invalidate_compose();
        }
    }

    pub fn toc_filter_start(&mut self) {
        if self.show_toc {
            self.toc_filter.clear();
            self.toc_filter_editing = true;
            self.toc_cursor = 0;
            self.invalidate_compose();
        }
    }

    pub fn toc_filter_finish(&mut self) {
        self.toc_filter_editing = false;
        self.invalidate_compose();
    }

    pub fn cycle_color(&mut self) {
        self.selection_color_idx = (self.selection_color_idx + 1) % HIGHLIGHT_COLORS.len();
        let color = HIGHLIGHT_COLORS[self.selection_color_idx];
        self.status = format!("color: {}", color.name);
        self.invalidate_compose();
    }

    /// Move the head caret by `delta` chars within the current page.
    /// Negative deltas walk backwards. Phase 1 stays on a single
    /// page; cross-page motion lands in Phase 3 along with `gj`/`gk`.
    pub fn move_head_chars(&mut self, delta: i32) {
        if let Some(sel) = self.text_selection.as_mut() {
            let Some(pt) = self.text_cache.get(sel.head.page) else {
                return;
            };
            let n = pt.chars.len() as i32;
            if n == 0 {
                return;
            }
            let new = ((sel.head.idx as i32) + delta).clamp(0, n - 1);
            sel.head.idx = new as usize;
        }
    }

    /// Move the head caret to the previous/next visual line, keeping
    /// roughly the same x-column (vim's `j`/`k` over wrapped lines).
    pub fn move_head_lines(&mut self, delta: i32) {
        if let Some(sel) = self.text_selection.as_mut() {
            let Some(pt) = self.text_cache.get(sel.head.page) else {
                return;
            };
            // Bail before computing line-clamps if `pt.lines` is
            // empty — `clamp(0, -1)` panics with "min > max" when
            // `pt.lines.len() == 0`. line_of returning Some implies
            // non-empty, but a stale snapshot from a re-loaded text
            // layout could violate that.
            if pt.lines.is_empty() {
                return;
            }
            let cur_line = match pt.line_of(sel.head.idx) {
                Some(l) => l as i32,
                None => return,
            };
            let max_line = pt.lines.len().saturating_sub(1) as i32;
            let new_line = (cur_line + delta).clamp(0, max_line) as usize;
            // Pick the char on the new line whose origin_x is nearest
            // to the current caret's origin_x. The `[start_idx, end_idx]`
            // span is in *stream* order — pdfium can interleave chars
            // from neighbouring lines (footnote/marginalia/multi-column
            // pages) — so filter by `c.line == new_line` or the caret
            // can teleport to a different visual line.
            // Use `get` instead of direct index — `sel.head.idx` is
            // persisted across page re-loads (LRU eviction → reload
            // of the same page may produce a shorter `chars` if the
            // pdfium build differs), so the index can outlive its
            // backing data.
            let target_x = match pt.chars.get(sel.head.idx) {
                Some(c) => c.origin_x,
                None => return,
            };
            // Use `lines.get(new_line)` for the same reason as
            // `pt.chars.get(sel.head.idx)` above — a re-loaded page can
            // yield a shorter `lines`/`chars` than the cached spans
            // claim. `i` is allowed to drift past `chars.len()` if a
            // line span over-counts; bail to defensive `chars.get(i)`
            // rather than direct index.
            let Some(span) = pt.lines.get(new_line) else {
                return;
            };
            let span_start = span.start_idx;
            let span_end = span.end_idx;
            let mut best = (span_start, f32::MAX);
            for i in span_start..=span_end {
                let Some(c) = pt.chars.get(i) else { break };
                if c.line != new_line {
                    continue;
                }
                let dx = (c.origin_x - target_x).abs();
                if dx < best.1 {
                    best = (i, dx);
                }
            }
            // Only commit if at least one matching-line char was found.
            // Without this guard, a multi-column / interleaved-stream
            // page where no `c.line == new_line` falls inside the span
            // leaves `best.1 == f32::MAX` and `best.0 == span_start` —
            // and `chars[span_start].line` may not equal `new_line`,
            // producing the exact teleport-to-wrong-line glitch this
            // function is supposed to prevent.
            if best.1 != f32::MAX {
                sel.head.idx = best.0;
            }
        }
    }

    /// Move head to the next word start (`w`).
    pub fn move_head_word_forward(&mut self) {
        if let Some(sel) = self.text_selection.as_mut() {
            if let Some(pt) = self.text_cache.get(sel.head.page) {
                sel.head.idx = pt.next_word_start(sel.head.idx);
            }
        }
    }

    /// Move head to the previous word start (`b`).
    pub fn move_head_word_back(&mut self) {
        if let Some(sel) = self.text_selection.as_mut() {
            if let Some(pt) = self.text_cache.get(sel.head.page) {
                sel.head.idx = pt.prev_word_start(sel.head.idx);
            }
        }
    }

    /// Move head to end of word (`e`).
    pub fn move_head_word_end(&mut self) {
        if let Some(sel) = self.text_selection.as_mut() {
            if let Some(pt) = self.text_cache.get(sel.head.page) {
                sel.head.idx = pt.end_of_word(sel.head.idx);
            }
        }
    }

    /// Move head to first char of current line (`0`).
    pub fn move_head_line_start(&mut self) {
        if let Some(sel) = self.text_selection.as_mut() {
            if let Some(pt) = self.text_cache.get(sel.head.page) {
                if let Some(line) = pt.line_of(sel.head.idx) {
                    if let Some(start) = pt.line_start(line) {
                        sel.head.idx = start;
                    }
                }
            }
        }
    }

    /// First non-blank on current line (`^`).
    pub fn move_head_line_first_nonblank(&mut self) {
        if let Some(sel) = self.text_selection.as_mut() {
            if let Some(pt) = self.text_cache.get(sel.head.page) {
                if let Some(line) = pt.line_of(sel.head.idx) {
                    if let Some(idx) = pt.line_first_non_blank(line) {
                        sel.head.idx = idx;
                    }
                }
            }
        }
    }

    /// End of current line (`$`).
    pub fn move_head_line_end(&mut self) {
        if let Some(sel) = self.text_selection.as_mut() {
            if let Some(pt) = self.text_cache.get(sel.head.page) {
                if let Some(line) = pt.line_of(sel.head.idx) {
                    if let Some(end) = pt.line_end(line) {
                        sel.head.idx = end;
                    }
                }
            }
        }
    }

    /// `gg` / `G` over text — first/last char of the current page.
    /// Cross-page caret motion is intentionally deferred: `j`/`k` and
    /// `w`/`b` still respect page boundaries; document-wide motion is
    /// what `Page Down` / `:goto N` are for.
    pub fn move_head_page_top(&mut self) {
        if let Some(sel) = self.text_selection.as_mut() {
            if !self
                .text_cache
                .get(sel.head.page)
                .map(|pt| pt.chars.is_empty())
                .unwrap_or(true)
            {
                sel.head.idx = 0;
            }
        }
    }

    pub fn move_head_page_bottom(&mut self) {
        if let Some(sel) = self.text_selection.as_mut() {
            if let Some(pt) = self.text_cache.get(sel.head.page) {
                if !pt.chars.is_empty() {
                    sel.head.idx = pt.chars.len() - 1;
                }
            }
        }
    }

    /// `f<c>` — jump to the next occurrence of `c` on the current line.
    pub fn move_head_find_char(&mut self, target: char, forward: bool) {
        if let Some(sel) = self.text_selection.as_mut() {
            if let Some(pt) = self.text_cache.get(sel.head.page) {
                let new = if forward {
                    pt.find_char_in_line(sel.head.idx, target)
                } else {
                    pt.rfind_char_in_line(sel.head.idx, target)
                };
                if let Some(idx) = new {
                    sel.head.idx = idx;
                }
            }
        }
    }

    /// `iw` — set selection to the word containing the head.
    /// Clears placement mode: the user explicitly issued an
    /// extending operator, so the next caret motion should grow the
    /// selection rather than collapse anchor=head as it would in
    /// placement mode (`sync_anchor_to_head_if_placing`).
    pub fn select_inner_word(&mut self) {
        if let Some(sel) = self.text_selection.as_mut() {
            if let Some(pt) = self.text_cache.get(sel.head.page) {
                if let Some((s, e)) = pt.word_around(sel.head.idx) {
                    sel.anchor.idx = s;
                    sel.head.idx = e;
                    sel.anchor.page = sel.head.page;
                }
            }
        }
        self.selection_placement = false;
    }

    /// `is` — sentence around the head. See `select_inner_word` for
    /// why this clears placement mode.
    pub fn select_inner_sentence(&mut self) {
        if let Some(sel) = self.text_selection.as_mut() {
            if let Some(pt) = self.text_cache.get(sel.head.page) {
                if let Some((s, e)) = pt.sentence_around(sel.head.idx) {
                    sel.anchor.idx = s;
                    sel.head.idx = e;
                    sel.anchor.page = sel.head.page;
                }
            }
        }
        self.selection_placement = false;
    }

    /// `ip` — paragraph around the head. See `select_inner_word` for
    /// why this clears placement mode.
    pub fn select_inner_paragraph(&mut self) {
        if let Some(sel) = self.text_selection.as_mut() {
            if let Some(pt) = self.text_cache.get(sel.head.page) {
                if let Some((s, e)) = pt.paragraph_around(sel.head.idx) {
                    sel.anchor.idx = s;
                    sel.head.idx = e;
                    sel.anchor.page = sel.head.page;
                }
            }
        }
        self.selection_placement = false;
    }

    /// `V` — toggle linewise. If already Linewise, reverts to
    /// Charwise so users can recover (vim's `V` toggle semantics).
    /// The anchor and head still point at chars; the renderer
    /// expands them to whole lines while in Linewise mode.
    /// Clears placement mode — pressing `V` while placing the caret
    /// implies the user wants a band, but the placement-mode skip
    /// in `bake_selection_into_page` would suppress it otherwise.
    pub fn enter_visual_line(&mut self) {
        if let Some(sel) = self.text_selection.as_mut() {
            sel.mode = if matches!(sel.mode, SelMode::Linewise) {
                SelMode::Charwise
            } else {
                SelMode::Linewise
            };
        } else {
            self.enter_visual();
            if let Some(sel) = self.text_selection.as_mut() {
                sel.mode = SelMode::Linewise;
            }
        }
        self.selection_placement = false;
    }

    /// `<C-v>` — toggle blockwise (rectangular). If already
    /// Blockwise, reverts to Charwise (vim semantics). Renderer is
    /// char-rectangle (per-line slice from the leftmost selected
    /// column to the rightmost on each row covered by the selection).
    /// Clears placement mode for the same reason `V` does.
    pub fn enter_visual_block(&mut self) {
        if let Some(sel) = self.text_selection.as_mut() {
            sel.mode = if matches!(sel.mode, SelMode::Blockwise) {
                SelMode::Charwise
            } else {
                SelMode::Blockwise
            };
        } else {
            self.enter_visual();
            if let Some(sel) = self.text_selection.as_mut() {
                sel.mode = SelMode::Blockwise;
            }
        }
        self.selection_placement = false;
    }

    /// Allocate the next group id and bump the counter.
    pub fn alloc_highlight_group_id(&mut self) -> u64 {
        let id = self.next_highlight_group_id;
        self.next_highlight_group_id = self.next_highlight_group_id.wrapping_add(1);
        id
    }

    /// Step 1 of the two-stroke delete: stage a confirm-prompt and
    /// return how many highlight rects would be wiped on the current
    /// page. The keys layer renders the prompt in the status line and
    /// listens for the confirming `y`. Returns 0 if there's nothing
    /// to delete on this page (caller surfaces "no highlights here").
    pub fn request_delete_last_highlight(&mut self) -> usize {
        let page = self.current_page();
        let target_group = self.last_highlight_group_on_page(page);
        let count = match target_group {
            // Grouped: count all entries on this page sharing the id.
            Some(g) => self
                .highlights
                .items
                .iter()
                .filter(|h| h.page == page && h.group_id == Some(g))
                .count(),
            // Legacy: there's still SOMETHING here, but no group id —
            // we'll fall back to deleting just the last entry.
            None => {
                if self.highlights.items.iter().any(|h| h.page == page) {
                    1
                } else {
                    0
                }
            }
        };
        self.awaiting_highlight_delete_confirm = count != 0;
        count
    }

    /// Step 2: actually delete the staged highlight group on the
    /// current page. Returns the number of rects removed (0 if there
    /// was nothing to delete or the request was already cancelled).
    pub fn confirm_delete_last_highlight(&mut self) -> usize {
        if !self.awaiting_highlight_delete_confirm {
            return 0;
        }
        self.awaiting_highlight_delete_confirm = false;
        let page = self.current_page();
        let target_group = self.last_highlight_group_on_page(page);
        let before = self.highlights.items.len();
        match target_group {
            Some(g) => self
                .highlights
                .items
                .retain(|h| !(h.page == page && h.group_id == Some(g))),
            None => {
                if let Some(idx) = self.highlights.items.iter().rposition(|h| h.page == page) {
                    self.highlights.remove_at(idx);
                }
            }
        }
        let removed = before.saturating_sub(self.highlights.items.len());
        if removed > 0 {
            // The retain branch above bypasses the per-page revision
            // counter (Vec::retain can't route through remove_at). Bump
            // explicitly here — only `page` is ever affected by either
            // branch, so one bump covers both.
            self.highlights.bump_page_revision(page);
            self.highlight_revision += 1;
            self.invalidate_compose();
            // Mirror the auto-persist on the yank path so a panic
            // between delete and exit doesn't resurrect the deleted
            // highlight on next reopen.
            if let Err(e) = self.persist_highlights() {
                self.status = format!(
                    "deleted in memory; auto-persist failed: {}",
                    crate::term_safe::safe_for_stderr(&format!("{e:#}"))
                );
            }
        }
        removed
    }

    pub fn cancel_delete_last_highlight(&mut self) {
        if self.awaiting_highlight_delete_confirm {
            self.awaiting_highlight_delete_confirm = false;
            // Without overwriting `status`, the "press y to confirm…"
            // prompt lingers indefinitely until the next status-bearing
            // event, leaving the user uncertain whether the cancel
            // actually registered.
            self.status = "highlight delete cancelled".into();
        }
    }

    /// Find the group_id of the most-recently-added highlight on
    /// `page`. Walks from the end of the items vec; first match wins.
    /// Returns None if there's a highlight on the page but it has no
    /// group_id (legacy / migrated entry — the caller falls back to
    /// "delete one entry").
    fn last_highlight_group_on_page(&self, page: usize) -> Option<u64> {
        for h in self.highlights.items.iter().rev() {
            if h.page == page {
                return h.group_id;
            }
        }
        None
    }

    /// Convert a terminal (column, row) cell coordinate into
    /// `(page_idx, normalised_x_in_page, normalised_y_in_page)`.
    /// Returns `None` if the cell is outside the image area or below
    /// the last page (i.e. inside an inter-page gap or the empty
    /// area past the doc end). Thin wrapper over `cell_to_page_coord_pure`
    /// — the pure version is what unit tests exercise.
    pub fn cell_to_page_coord(&self, col: u16, row: u16) -> Option<(usize, f32, f32)> {
        cell_to_page_coord_pure(
            col,
            row,
            self.image_area,
            self.cell_size_px,
            self.viewport_px,
            self.scroll_x,
            self.scroll_y_px,
            &self.layout,
            self.page_count,
        )
    }

    /// Begin a mouse-drag text selection. Loads the page's text
    /// layout if not cached, finds the char nearest the click point,
    /// and anchors the selection there. Cross-page drag is supported
    /// — the head simply moves to whichever page the cursor hovers.
    pub fn mouse_drag_start(&mut self, col: u16, row: u16) {
        let Some((page, nx, ny)) = self.cell_to_page_coord(col, row) else {
            return;
        };
        let Some(metrics) = self.page_metrics.get(page).copied() else {
            return;
        };
        let pt = match self.text_cache.get_or_load(&self.document, page, &metrics) {
            Ok(pt) => pt,
            Err(_) => return,
        };
        let Some(idx) = pt.char_at_point(nx, ny) else {
            return;
        };
        let caret = Caret { page, idx };
        self.text_selection = Some(TextSelection::new(caret));
        self.mouse_dragging = true;
        self.mode = Mode::Visual;
        // Mouse drag is an explicit "I'm selecting NOW" gesture; skip
        // placement mode entirely so the band starts growing from the
        // very first drag delta.
        self.selection_placement = false;
        self.clear_chord_state();
        self.status = "Drag to select · release to save · Esc to cancel".into();
    }

    /// Update the head caret while the mouse is held. Crosses pages
    /// freely — `range_to_rects` + `extract` handle multi-page spans.
    pub fn mouse_drag_to(&mut self, col: u16, row: u16) {
        if !self.mouse_dragging {
            return;
        }
        let Some((page, nx, ny)) = self.cell_to_page_coord(col, row) else {
            return;
        };
        let Some(metrics) = self.page_metrics.get(page).copied() else {
            return;
        };
        let pt = match self.text_cache.get_or_load(&self.document, page, &metrics) {
            Ok(pt) => pt,
            Err(_) => return,
        };
        let Some(idx) = pt.char_at_point(nx, ny) else {
            return;
        };
        if let Some(sel) = self.text_selection.as_mut() {
            sel.head = Caret { page, idx };
        }
        // Mouse drags fire at 60+ Hz and `dispatch_event_coalesced`
        // drops every drag but the latest in a burst. A fast page
        // 2 → page 4 sweep therefore never invokes this function on
        // the intermediate page 3 — `text_cache` ends up holding
        // text for pages 2 and 4 only. The render path
        // (`fill_selection_overlay_image` reachable via ui.rs:732
        // `app.text_cache.get(b.page_idx)`) silently skips pages
        // without loaded text, so page 3's selection band never
        // paints; worse, `yank_selection` (app.rs:1322) skips them
        // too, so the saved highlight + clipboard text are missing
        // page 3 entirely. Force-load text for every page in the
        // current selection span so both the live overlay and the
        // eventual yank see every page's chars.
        self.ensure_selection_text_loaded();
    }

    /// Synchronously load `PageText` for every page touched by the
    /// active selection (`lo.page..=hi.page`). No-op when no selection
    /// is active or when a page's text load fails (rare; pdfium errors
    /// are logged via the same path as elsewhere).
    fn ensure_selection_text_loaded(&mut self) {
        let Some(sel) = self.text_selection else {
            return;
        };
        let (lo, hi) = sel.ordered();
        let pages_to_load =
            pages_needing_text_load(lo.page, hi.page, self.page_metrics.len(), |p| {
                self.text_cache.get(p).is_some()
            });
        for page_idx in pages_to_load {
            let Some(metrics) = self.page_metrics.get(page_idx).copied() else {
                continue;
            };
            // Discard the &PageText return; we only care that it gets
            // inserted into the cache. Errors are swallowed — a
            // missing page in the cache will simply skip rendering /
            // yanking that page, which is the prior behaviour.
            let _ = self
                .text_cache
                .get_or_load(&self.document, page_idx, &metrics);
        }
    }

    /// Finalise a mouse-drag selection. Treats a click-without-drag
    /// (anchor == head) as "exit Visual mode without saving"; any
    /// real range commits as a highlight + clipboard yank.
    pub fn mouse_drag_end(&mut self) {
        if !self.mouse_dragging {
            return;
        }
        self.mouse_dragging = false;
        let real = self
            .text_selection
            .map(|s| s.anchor != s.head)
            .unwrap_or(false);
        if real {
            self.save_selection();
        } else {
            self.exit_visual();
        }
    }

    pub fn persist_highlights(&mut self) -> Result<()> {
        // Skip the save entirely when the user didn't touch a highlight
        // this session. `save_to_pdf` walks every page in the document
        // (pdfium has no per-doc annotation index) and on a 700-page
        // book that's a 5–7 s pause after `:q`. Read-only sessions are
        // common — a no-op short-circuit makes them quit instantly.
        //
        // Safe because the on-disk annotations were the source of truth
        // at load time, the in-memory store is the unedited mirror, and
        // the PDF file hasn't been touched since. Any external editor
        // that modified the PDF mid-session would race with our save
        // anyway.
        if self.highlight_revision == 0 {
            return Ok(());
        }
        // Per-page filtered save: only walk pages that either had our
        // annotations at load time (so we can delete) or have highlights
        // now (so we can add). Pages in neither set are guaranteed
        // untouched. On a 700-page book with a single new highlight
        // this trims the save from ~7 s to ~50 ms.
        let now_pages: std::collections::HashSet<usize> =
            self.highlights.items.iter().map(|h| h.page).collect();
        let candidate: std::collections::HashSet<usize> = self
            .prev_highlight_pages
            .union(&now_pages)
            .copied()
            .collect();
        let result = pdfhighlights::save_to_pdf_filtered(
            &self.document,
            &self.highlights,
            &self.path,
            &candidate,
        );
        // After a successful save, the on-disk truth is the in-memory
        // set — refresh `prev_highlight_pages` so a subsequent save
        // (e.g. user calls `:w` and keeps editing) doesn't redundantly
        // re-walk pages that no longer hold any of our annotations.
        // Cheap; the HashSet is tiny.
        if result.is_ok() {
            self.prev_highlight_pages = now_pages;
        }
        result
    }

    /// Build a `StatefulProtocol` for the supplied canvas. For the
    /// kitty protocol we bypass `picker.new_resize_protocol` so we
    /// can hand-pick a stable image ID — this is the fix for the
    /// Ghostty crash at ~10–20 keystrokes (every random ID stayed
    /// resident in the terminal's image store).
    pub fn build_protocol(&self, canvas: DynamicImage) -> StatefulProtocol {
        match self.picker.protocol_type() {
            ProtocolType::Kitty => {
                let source = ImageSource::new(canvas, self.picker.font_size(), Rgba([0, 0, 0, 0]));
                let kitty = StatefulKitty::new(self.kitty_image_id, self.is_tmux);
                StatefulProtocol::new(
                    source,
                    self.picker.font_size(),
                    StatefulProtocolType::Kitty(kitty),
                )
            }
            // Other protocols don't have the same image-id-namespace
            // problem (sixel/iterm2 stream the bytes inline; halfblocks
            // is just text). Use the picker's stock builder.
            _ => self.picker.new_resize_protocol(canvas),
        }
    }

    pub fn persist_session(&self) -> Result<()> {
        // Capture the user's exact within-page reading position as a
        // fraction so a reopen lands at the same scroll, not just the
        // top of the page they were on. Mirrors the across-zoom math
        // in `ensure_layout`.
        let cur_page = self.current_page();
        let page_y = self.layout.page_y(cur_page);
        let page_h = self.layout.page_h(cur_page).max(1) as f32;
        let scroll_in_page = ((self.scroll_y_px - page_y) as f32 / page_h).clamp(0.0, 1.0);
        Session {
            page: cur_page,
            dark: self.dark,
            zoom: self.zoom,
            marks: self.marks.clone(),
            scroll_in_page,
            selection_color_idx: self.selection_color_idx,
        }
        .save(&self.path)
    }

    /// Build a one-line PDF metadata summary for the status bar.
    /// Pulls title / author from pdfium and adds page count + file size.
    /// Title and author are attacker-controlled (a malicious PDF can
    /// embed terminal escape sequences in either field to hijack the
    /// status line — OSC 52 clipboard write, alt-screen toggle, etc.);
    /// run them through `term_safe::safe_for_stderr` before they hit
    /// any render path that goes to the wire.
    pub fn show_info(&mut self) {
        use pdfium_render::prelude::PdfDocumentMetadataTagType;
        let meta = self.document.metadata();
        let title = meta
            .get(PdfDocumentMetadataTagType::Title)
            .map(|t| crate::term_safe::safe_for_stderr(t.value()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                // Filename fallback when PDF Title is empty.
                // Filenames are user-supplied via the shell; a
                // hostile-named file (`/tmp/evil\u{202E}fdp.gif`)
                // can leak bidi/separator chars into the status
                // line. Run through the same sanitiser the title /
                // author paths above use.
                crate::term_safe::safe_for_stderr(
                    self.path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("(unknown)"),
                )
            });
        let author = meta
            .get(PdfDocumentMetadataTagType::Author)
            .map(|t| crate::term_safe::safe_for_stderr(t.value()))
            .filter(|s| !s.is_empty());
        let bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        self.status = match author {
            Some(a) => format!(
                "{} — {} · {} pages · {}",
                title,
                a,
                self.page_count,
                human_bytes(bytes),
            ),
            None => format!(
                "{} · {} pages · {}",
                title,
                self.page_count,
                human_bytes(bytes),
            ),
        };
    }

    /// Build a one-line render-pipeline diagnostic for the status bar.
    /// Useful for chasing blur / sizing bugs ("am I rendering at the
    /// resolution I think I am?") without bouncing to env vars or logs.
    pub fn show_diag(&mut self) {
        let (cw, ch) = self.cell_size_px;
        let (vw, vh) = self.viewport_px;
        let cells_w = if cw == 0 { 0 } else { vw / cw as u32 };
        let cells_h = if ch == 0 { 0 } else { vh / ch as u32 };
        let scale = std::env::var("TERMPDF_RENDER_SCALE")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(2.0);
        let proto = match self.picker.protocol_type() {
            ratatui_image::picker::ProtocolType::Kitty => "kitty",
            ratatui_image::picker::ProtocolType::Sixel => "sixel",
            ratatui_image::picker::ProtocolType::Iterm2 => "iterm2",
            ratatui_image::picker::ProtocolType::Halfblocks => "halfblocks",
        };
        self.status = format!(
            "{}/{} cell={}x{}px viewport={}x{}cell ({}x{}px) fit_w={}px zoom={:.2}× scale={:.1}× dark={}",
            proto,
            if self.is_tmux { "tmux" } else { "direct" },
            cw, ch,
            cells_w, cells_h,
            vw, vh,
            self.layout.fit_width_px,
            self.zoom,
            scale,
            self.dark,
        );
    }

    /// Set mark `c` to the currently-leading page. Vim's marks are
    /// scoped per buffer; ours are per PDF and persisted via Session.
    pub fn set_mark(&mut self, c: char) {
        if !c.is_ascii_lowercase() {
            self.status = format!("marks must be a-z, got {c:?}");
            return;
        }
        let p = self.current_page();
        self.marks.insert(c, p);
        self.status = format!("mark {c} set to page {}", p + 1);
    }

    /// Jump to mark `c`. Records the source page in the jumplist so
    /// `<C-o>` can return.
    pub fn jump_to_mark(&mut self, c: char) {
        match self.marks.get(&c).copied() {
            Some(p) if p < self.page_count => {
                // goto_page records the jumplist for any non-trivial
                // delta; no need for a separate push_jump here.
                self.goto_page(p);
                self.status = format!("'{c} → page {}", p + 1);
            }
            Some(_) => self.status = format!("mark '{c}' points past end of doc"),
            None => self.status = format!("no mark '{c}'"),
        }
    }

    /// Append `from` to the jumplist (truncating any forward history),
    /// then position the cursor at the end. Mirrors vim semantics:
    /// after a fresh jump, `<C-i>` redo is gone.
    pub fn push_jump(&mut self, from: usize) {
        self.jumplist
            .truncate(self.jump_idx.min(self.jumplist.len()));
        self.jumplist.push(from);
        self.jump_idx = self.jumplist.len();
        // Bound the list so a long session doesn't grow without limit.
        const MAX_JUMPS: usize = 100;
        if self.jumplist.len() > MAX_JUMPS {
            let drop = self.jumplist.len() - MAX_JUMPS;
            self.jumplist.drain(..drop);
            self.jump_idx = self.jumplist.len();
        }
    }

    /// Walk backwards (`<C-o>`) through the jumplist. The first
    /// invocation pushes the current page so `<C-i>` can return.
    pub fn jump_back(&mut self) {
        if self.jumplist.is_empty() {
            self.status = "jumplist empty".into();
            return;
        }
        // On first walk-back, snapshot where we are now so <C-i> can
        // come back. Detect "first walk" by jump_idx == jumplist.len().
        if self.jump_idx == self.jumplist.len() {
            let here = self.current_page();
            if self.jumplist.last().copied() != Some(here) {
                self.jumplist.push(here);
                // Don't bump jump_idx — we're about to step back from
                // the end, into the entry just before this snapshot.
            }
        }
        if self.jump_idx == 0 {
            self.status = "at oldest jump".into();
            return;
        }
        self.jump_idx -= 1;
        let target = self.jumplist[self.jump_idx];
        self.goto_page_no_record(target);
        self.status = format!("jump back → page {}", target + 1);
    }

    /// Walk forward (`<C-i>`) through the jumplist.
    pub fn jump_forward(&mut self) {
        if self.jump_idx + 1 >= self.jumplist.len() {
            self.status = "at newest jump".into();
            return;
        }
        self.jump_idx += 1;
        let target = self.jumplist[self.jump_idx];
        self.goto_page_no_record(target);
        self.status = format!("jump forward → page {}", target + 1);
    }

    /// Yank the active selection as a Markdown blockquote with a
    /// trailing citation: `> line\n> line\n\n— filename, page N`.
    /// Pulled out of `yank_selection` so the regular y/Y paths stay
    /// plain-text. Sets status; clears Visual mode.
    pub fn yank_selection_as_markdown(&mut self) {
        // Mirror `yank_selection`: pull all pages in the selection range
        // into `text_cache` BEFORE consuming the selection. Otherwise a
        // page-2→page-4 cross-page mouse drag silently drops page 3
        // because that page's text was never cached during the drag.
        self.ensure_selection_text_loaded();
        let Some(sel) = self.text_selection.take() else {
            self.mode = Mode::Normal;
            return;
        };
        let (lo, hi) = sel.ordered();
        let mut combined = String::new();
        for page_idx in lo.page..=hi.page {
            let Some(pt) = self.text_cache.get(page_idx) else {
                continue;
            };
            let mut start = if page_idx == lo.page { lo.idx } else { 0 };
            let mut end = if page_idx == hi.page {
                hi.idx
            } else {
                pt.chars.len().saturating_sub(1)
            };
            // Linewise widening — same fix as `yank_selection`. The
            // markdown blockquote should match what the user sees
            // selected on screen.
            if matches!(sel.mode, SelMode::Linewise) {
                if let Some(line) = pt.line_of(start) {
                    if let Some(s) = pt.line_start(line) {
                        start = s;
                    }
                }
                if let Some(line) = pt.line_of(end) {
                    if let Some(e) = pt.line_end(line) {
                        end = e;
                    }
                }
            }
            let s = pt.extract(start, end);
            if !s.is_empty() {
                if !combined.is_empty() {
                    combined.push_str("\n\n");
                }
                combined.push_str(&s);
            }
        }
        let text = combined.trim();
        if text.is_empty() {
            self.status = "selection has no text (image-only?)".into();
            self.mouse_dragging = false;
            self.mode = Mode::Normal;
            return;
        }
        let quoted: String = text
            .lines()
            .map(|l| {
                if l.is_empty() {
                    "> ".into()
                } else {
                    format!("> {l}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let stem = self
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("document.pdf");
        let cite_page = lo.page + 1;
        let cite_range = if hi.page > lo.page {
            format!("pp. {}–{}", cite_page, hi.page + 1)
        } else {
            format!("p. {cite_page}")
        };
        let payload = format!("{quoted}\n\n— {stem}, {cite_range}\n");
        let outcome = crate::clipboard::copy(&payload);
        self.mouse_dragging = false;
        self.mode = Mode::Normal;
        self.status = if outcome.truncated {
            format!(
                "copied markdown quote ({}, truncated by clipboard)",
                cite_range
            )
        } else {
            format!("copied markdown quote ({})", cite_range)
        };
    }

    /// Walk every saved highlight and produce a Markdown notes file
    /// at `out_path`. One entry per highlight, page-grouped, with the
    /// quoted text pulled from the page's text layer if available.
    pub fn export_notes(&mut self, out_path: &Path) -> Result<()> {
        self.export_notes_inner(out_path, false)
    }

    /// Variant that overwrites an existing file. Wired to `:export!` so
    /// the user has to opt in (vim convention) — accidentally typing
    /// `:export ~/.bashrc` should fail loudly, not wipe the target.
    pub fn export_notes_force(&mut self, out_path: &Path) -> Result<()> {
        self.export_notes_inner(out_path, true)
    }

    fn export_notes_inner(&mut self, out_path: &Path, force_overwrite: bool) -> Result<()> {
        use std::io::Write;
        if self.highlights.items.is_empty() {
            anyhow::bail!("no highlights to export");
        }
        let stem = self
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("document.pdf");
        // Group by page for a clean, scannable output. Clone so we
        // don't hold an immutable borrow on `self.highlights` while
        // calling the mutating `text_for_highlight` below.
        let mut by_page: std::collections::BTreeMap<usize, Vec<Highlight>> =
            std::collections::BTreeMap::new();
        for h in &self.highlights.items {
            by_page.entry(h.page).or_default().push(h.clone());
        }
        let mut buf = String::new();
        buf.push_str(&format!("# Notes — {stem}\n\n"));
        for (page, items) in by_page {
            buf.push_str(&format!("## Page {}\n\n", page + 1));
            for h in items {
                let text = self
                    .text_for_highlight(&h)
                    .unwrap_or_else(|| "(image region — no extractable text)".to_string());
                let quoted: String = text
                    .lines()
                    .map(|l| {
                        if l.is_empty() {
                            "> ".into()
                        } else {
                            format!("> {l}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                buf.push_str(&quoted);
                buf.push_str("\n\n");
                if let Some(note) = &h.note {
                    buf.push_str(&format!("**Note:** {note}\n\n"));
                }
            }
        }
        // Path safety: open with O_NOFOLLOW so a planted symlink at
        // `out_path` doesn't redirect the write to a victim file
        // (~/.bashrc, etc.). On unix, also use O_EXCL by default; only
        // pass `truncate(true)` when the user has explicitly opted in
        // via `:export!`. Without these defenses, `:export ~/.bashrc`
        // (or a symlink at the auto-derived `<stem>.notes.md` next
        // to the PDF in a writable shared dir) silently wipes the
        // target file.
        #[cfg(unix)]
        let mut f = {
            use std::os::unix::fs::OpenOptionsExt;
            const O_NOFOLLOW: i32 = 0x20000;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).custom_flags(O_NOFOLLOW).mode(0o644);
            if force_overwrite {
                opts.create(true).truncate(true);
            } else {
                opts.create_new(true);
            }
            opts.open(out_path).with_context(|| {
                if force_overwrite {
                    format!("opening {} for write", out_path.display())
                } else {
                    format!(
                        "creating {} (use `:export!` to overwrite)",
                        out_path.display()
                    )
                }
            })?
        };
        #[cfg(not(unix))]
        let mut f = {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true);
            if force_overwrite {
                opts.create(true).truncate(true);
            } else {
                opts.create_new(true);
            }
            opts.open(out_path).with_context(|| {
                if force_overwrite {
                    format!("opening {} for write", out_path.display())
                } else {
                    format!(
                        "creating {} (use `:export!` to overwrite)",
                        out_path.display()
                    )
                }
            })?
        };
        f.write_all(buf.as_bytes())
            .with_context(|| format!("writing {}", out_path.display()))?;
        Ok(())
    }

    /// Best-effort text extraction for a saved highlight: load the
    /// page's text layer, find chars whose bbox overlaps the
    /// highlight rect, and concatenate them. Returns None if no chars
    /// land inside the rect (image-only region).
    fn text_for_highlight(&mut self, h: &Highlight) -> Option<String> {
        let metrics = self.page_metrics.get(h.page).copied()?;
        let pt = self
            .text_cache
            .get_or_load(&self.document, h.page, &metrics)
            .ok()?;
        // We only need the first and last hits to feed `extract`, so
        // track them directly instead of materialising the full index
        // list. On a long highlight this avoids growing a Vec to
        // hundreds of usizes per call.
        let mut start: Option<usize> = None;
        let mut end: usize = 0;
        for (i, c) in pt.chars.iter().enumerate() {
            if c.is_generated {
                continue;
            }
            let cx = c.bbox.x + c.bbox.w * 0.5;
            let cy = c.bbox.y + c.bbox.h * 0.5;
            if cx >= h.x && cx <= h.x + h.w && cy >= h.y && cy <= h.y + h.h {
                if start.is_none() {
                    start = Some(i);
                }
                end = i;
            }
        }
        let start = start?;
        Some(pt.extract(start, end))
    }
}

/// Pick a stable, per-process kitty image ID. We just need *an* ID
/// that won't collide with whatever else might already be live in
/// the user's terminal — ratatui-image's default uses `random()`,
/// which is fine for one-shot creation but defeats reuse-by-ID.
/// Process ID hashed with the golden-ratio constant gives us a
/// well-spread u32 without pulling in `rand`. Kitty IDs are 1..=u32::MAX;
/// we bump 0 to 1 just in case.
/// Pure helper: scan an outline for the first entry whose title
/// matches a "references / bibliography" heading. Returns the
/// resolved page index, or `None` if no such entry exists.
///
/// Match is case-insensitive substring against a small set of
/// heading words — covers "References", "Bibliography", "Works
/// Cited", and similar variants. We deliberately don't get clever
/// with regex: a long-tail of academic books title their refs
/// section "Notes and References" or "Selected Bibliography" and
/// substring covers them all.
pub fn find_references_page(outline: &[OutlineEntry]) -> Option<usize> {
    const REF_KEYWORDS: &[&str] = &[
        "reference",   // singular and plural
        "bibliograph", // bibliography / bibliographies
        "works cited",
        "literature cited",
    ];
    for entry in outline {
        let lower = entry.title.to_lowercase();
        for kw in REF_KEYWORDS {
            if lower.contains(kw) {
                if let Some(p) = entry.page {
                    return Some(p);
                }
                // Title matched but no resolved page — keep searching;
                // some PDFs split section anchors across siblings.
                break;
            }
        }
    }
    None
}

/// Pure helper: given a sorted-deduped slice of outline page
/// indices, the user's current page, and a direction (+1 next,
/// -1 prev), returns the page to jump to. `None` means "no
/// neighbour in that direction". Caller guarantees `outline_pages`
/// is sorted ascending and deduped — `App::outline_pages_sorted`
/// is built that way once at startup, so the section-jump path
/// stays allocation-free.
pub fn next_section_target(
    outline_pages: &[usize],
    current_page: usize,
    dir: i32,
) -> Option<usize> {
    if dir > 0 {
        outline_pages.iter().copied().find(|p| *p > current_page)
    } else {
        // For `[[`: from a page mid-section, land on the section's
        // first page. From the section's first page, land on the
        // previous section's first page.
        outline_pages
            .iter()
            .copied()
            .rev()
            .find(|p| *p < current_page)
    }
}

/// True iff `url` uses a scheme we're willing to dispatch via
/// xdg-open. PDF link annotations are attacker-controlled, so the
/// allow-list is intentionally short: http(s) for browsers,
/// mailto: for mail clients. Everything else (file://, javascript:,
/// data:, ssh://, tel:, intent://, etc.) gets rejected — they're
/// each their own exfiltration / RCE vector through xdg-open's
/// MIME-handler dispatch.
pub(crate) fn is_safe_external_url(url: &str) -> bool {
    let lower_prefix = |s: &str| {
        url.len() >= s.len() && url.as_bytes()[..s.len()].eq_ignore_ascii_case(s.as_bytes())
    };
    lower_prefix("http://") || lower_prefix("https://") || lower_prefix("mailto:")
}

/// Trim PDF-derived strings before they hit the status bar. ratatui's
/// Span filters control chars at render time, but a 64 KiB title
/// would still wrap the bar. Keep status messages tight; if the URL
/// is overlong, show a prefix + "…".
pub(crate) fn truncate_for_status(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Compact byte-count formatter for `:info`. Picks the largest unit
/// that keeps the number ≤ 1024 and prints to 1 decimal (omitted for
/// bytes). 1234567 → "1.2 MB"; 999 → "999 B".
fn human_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

/// f32 → ordered key for sorting. f32 isn't `Ord`; this maps NaN to
/// u32::MAX so it sorts last and otherwise preserves IEEE order.
fn ordered_float(f: f32) -> u32 {
    if f.is_nan() {
        return u32::MAX;
    }
    let bits = f.to_bits();
    if (bits as i32) >= 0 {
        bits ^ 0x8000_0000
    } else {
        !bits
    }
}

fn stable_kitty_id() -> u32 {
    // PID alone collides on the kitty image-id namespace if two
    // termpdf instances run in the same Ghostty/tmux session — the
    // second instance's deletes (`a=d,d=R,x=lo,y=hi`) sweep the
    // first's page bitmaps, producing missing-image placeholders +
    // (per the 2026-05-04 incident) journald spam that crashes
    // Ghostty. Mix in nanoseconds-since-epoch so two instances
    // started in the same wall-clock second still get disjoint
    // namespaces. The XOR-fold to u32 is non-cryptographic but the
    // birthday-paradox collision space is ~2³² which is plenty for
    // a "user has a few PDFs open" workload.
    use std::time::{SystemTime, UNIX_EPOCH};
    let pid = std::process::id() as u64;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mixed = pid
        .wrapping_mul(0x9E37_79B1_185E_BCA1)
        .wrapping_add(nanos.wrapping_mul(0xBF58_476D_1CE4_E5B9));
    let id = ((mixed >> 32) ^ mixed) as u32;
    // Clamp into the lower half of u32 so `id_for(p) = id_base + 1
    // + p` cannot wrap past u32::MAX into low IDs (image_id 0 is
    // reserved by the kitty graphics protocol; wrapping there
    // silently drops placements). A 31-bit namespace still gives
    // ~2¹⁵ collisions on the birthday-paradox boundary — plenty
    // for a "few PDFs open" workload.
    (id & 0x7FFF_FFFF).max(1)
}

/// Inter-page gap in pixels. Small but visible — large enough that
/// users notice page boundaries when scrolling, small enough not to
/// waste vertical space.
pub fn layout_gap_px() -> u32 {
    8
}

/// Pure helper used by `App::ensure_selection_text_loaded` to decide
/// which page indices in the inclusive selection span `[lo_page,
/// hi_page]` need a synchronous `text_cache.get_or_load`. Filters
/// out pages already present in the cache and pages past the
/// document's `page_count`. Extracted so the cross-page-drag fix
/// has a regression test that doesn't need a loaded `PdfDocument`.
pub fn pages_needing_text_load(
    lo_page: usize,
    hi_page: usize,
    page_count: usize,
    has_text: impl Fn(usize) -> bool,
) -> Vec<usize> {
    if page_count == 0 || lo_page > hi_page {
        return Vec::new();
    }
    let hi = hi_page.min(page_count.saturating_sub(1));
    let mut out = Vec::with_capacity(hi - lo_page + 1);
    for p in lo_page..=hi {
        if !has_text(p) {
            out.push(p);
        }
    }
    out
}

/// Pure version of `App::first_visible_char_idx`. Given the page's
/// viewport-top in normalised page space (0 = page top, 1 = page
/// bottom), find the first non-generated char whose top edge sits at
/// or below the viewport top. Extracted so the bug it fixes — entering
/// Visual mode at idx 0 when the user has scrolled deep into a page
/// hides the live selection above the viewport — has a regression
/// test that doesn't need pdfium loaded.
pub fn first_visible_char_idx_pure(
    viewport_top_norm: f32,
    pt: &crate::textlayout::PageText,
) -> Option<usize> {
    for (i, c) in pt.chars.iter().enumerate() {
        if c.is_generated {
            continue;
        }
        if c.bbox.y >= viewport_top_norm {
            return Some(i);
        }
    }
    None
}

/// Pure version of `App::cell_to_page_coord`, extracted so the
/// many branches of the mouse-coord transform can be unit-tested
/// without constructing a `PdfDocument`.
pub fn cell_to_page_coord_pure(
    col: u16,
    row: u16,
    image_area: Rect,
    cell_size_px: (u16, u16),
    viewport_px: (u32, u32),
    scroll_x: f32,
    scroll_y_px: i64,
    layout: &PageLayout,
    page_count: usize,
) -> Option<(usize, f32, f32)> {
    if col < image_area.x || row < image_area.y {
        return None;
    }
    let local_col = col - image_area.x;
    let local_row = row - image_area.y;
    if local_col >= image_area.width || local_row >= image_area.height {
        return None;
    }
    let (cell_w, cell_h) = cell_size_px;
    let viewport_x = local_col as i64 * cell_w as i64;
    let viewport_y = local_row as i64 * cell_h as i64;

    let (vw, _) = viewport_px;
    let fw = layout.fit_width_px;
    let page_x_origin: i64 = if fw <= vw {
        ((vw - fw) / 2) as i64
    } else {
        -(((fw - vw) as f32) * scroll_x).round() as i64
    };
    let page_x = viewport_x - page_x_origin;
    if page_x < 0 || page_x >= fw as i64 {
        return None;
    }

    let doc_y = scroll_y_px + viewport_y;
    let page_idx = layout.page_at(doc_y);
    if page_idx >= page_count {
        return None;
    }
    let page_top = layout.page_y(page_idx);
    let page_h = layout.page_h(page_idx);
    if page_h == 0 {
        return None;
    }
    let local_y = doc_y - page_top;
    if local_y < 0 || local_y >= page_h as i64 {
        return None;
    }
    let nx = (page_x as f32 / fw as f32).clamp(0.0, 1.0);
    let ny = (local_y as f32 / page_h as f32).clamp(0.0, 1.0);
    Some((page_idx, nx, ny))
}

/// Pure helper: compute the new selection rectangle after a Visual
/// mode hjkl/HJKL keypress. Sliding leaves size fixed; resizing
/// grows/shrinks from the bottom-right corner. Both modes clamp to
/// stay inside the page and never collapse below 1% × 1%.
///
/// Kept around (with tests) as the reference implementation for the
/// future visual-block (`<C-v>`) rectangular-selection mode that
/// lands in Phase 4.
#[allow(dead_code)]
pub fn nudge_rect(sel: Rect01, dx: f32, dy: f32, resize: bool) -> Rect01 {
    if resize {
        Rect01 {
            x: sel.x,
            y: sel.y,
            w: (sel.w + dx).clamp(0.01, 1.0 - sel.x),
            h: (sel.h + dy).clamp(0.01, 1.0 - sel.y),
        }
    } else {
        Rect01 {
            x: (sel.x + dx).clamp(0.0, 1.0 - sel.w),
            y: (sel.y + dy).clamp(0.0, 1.0 - sel.h),
            w: sel.w,
            h: sel.h,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::PageMetrics;

    /// PDF link annotations are attacker-controlled; only http(s) and
    /// mailto: are safe to dispatch through xdg-open. file://,
    /// javascript:, data:, ssh://, tel: each become exfiltration or
    /// RCE vectors via xdg-open's MIME-handler dispatch.
    #[test]
    fn url_scheme_allowlist_rejects_dangerous_schemes() {
        assert!(is_safe_external_url("http://example.com"));
        assert!(is_safe_external_url("https://example.com/path?q=1"));
        assert!(is_safe_external_url("HTTPS://Example.COM"));
        assert!(is_safe_external_url("mailto:user@example.com"));
        assert!(is_safe_external_url("MailTo:user@example.com"));

        assert!(!is_safe_external_url("file:///etc/passwd"));
        assert!(!is_safe_external_url("javascript:alert(1)"));
        assert!(!is_safe_external_url("data:text/html,<script>"));
        assert!(!is_safe_external_url("ssh://host"));
        assert!(!is_safe_external_url("tel:+15551234"));
        assert!(!is_safe_external_url("intent://app"));
        // Defenses against argv flag-confusion at xdg-open: the URL
        // path also gets `--` argv-separated, but reject obvious
        // flag-shaped things at the gate too.
        assert!(!is_safe_external_url("--version"));
        assert!(!is_safe_external_url("-h"));
        assert!(!is_safe_external_url(""));
        // Almost-http: substring match must be anchored at the start.
        assert!(!is_safe_external_url(" https://example.com"));
        assert!(!is_safe_external_url("xhttp://example.com"));
    }

    #[test]
    fn truncate_for_status_drops_excess_with_ellipsis() {
        assert_eq!(truncate_for_status("short", 10), "short");
        assert_eq!(truncate_for_status("exactly_10", 10), "exactly_10");
        assert_eq!(truncate_for_status("0123456789abc", 10), "012345678…");
        // Multi-byte chars count as one each, not by byte length.
        let s = "αβγδεζηθικλμνξ";
        assert_eq!(truncate_for_status(s, 5).chars().count(), 5);
    }

    fn metrics(n: usize) -> Vec<PageMetrics> {
        // 100×200pt portrait pages.
        vec![
            PageMetrics {
                width_pts: 100.0,
                height_pts: 200.0
            };
            n
        ]
    }

    fn layout_for(n: usize, fit_width_px: u32) -> PageLayout {
        PageLayout::build(&metrics(n), fit_width_px, 8)
    }

    /// Regression test for the cross-page mouse-drag bug: a fast
    /// drag from page 2 → page 4 only invokes `mouse_drag_to` on the
    /// final page (the rest are coalesced away in
    /// `dispatch_event_coalesced`), so `text_cache` is missing the
    /// intermediate page 3. Without the fix, `yank_selection` (line
    /// ~1322) silently skips page 3 and the saved highlight +
    /// clipboard text omit it; with the fix, `pages_needing_text_load`
    /// returns page 3 so `ensure_selection_text_loaded` will load it.
    #[test]
    fn pages_needing_text_load_returns_intermediate_uncached_pages() {
        // Cache state mirrors a fast page-2 → page-4 drag: only the
        // start (page 2) and end (page 4) pages have text loaded.
        let cached: std::collections::HashSet<usize> = [2usize, 4].iter().copied().collect();
        let need = pages_needing_text_load(2, 4, 10, |p| cached.contains(&p));
        assert_eq!(need, vec![3], "page 3 must be flagged for load");
    }

    #[test]
    fn pages_needing_text_load_skips_already_cached() {
        let cached: std::collections::HashSet<usize> =
            [0usize, 1, 2, 3, 4].iter().copied().collect();
        let need = pages_needing_text_load(0, 4, 10, |p| cached.contains(&p));
        assert!(need.is_empty(), "all pages cached → no loads needed");
    }

    #[test]
    fn pages_needing_text_load_clamps_to_page_count() {
        // hi_page beyond doc end should not generate out-of-range
        // load attempts; the App-level loop also guards with
        // `page_metrics.get`, but the helper clamps too so callers
        // without that guard can't OOB.
        let cached: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let need = pages_needing_text_load(2, 99, 5, |p| cached.contains(&p));
        assert_eq!(need, vec![2, 3, 4]);
    }

    #[test]
    fn pages_needing_text_load_single_page_uncached_returns_it() {
        let cached: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let need = pages_needing_text_load(2, 2, 10, |p| cached.contains(&p));
        assert_eq!(need, vec![2]);
    }

    #[test]
    fn pages_needing_text_load_empty_doc_returns_empty() {
        let cached: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let need = pages_needing_text_load(0, 0, 0, |p| cached.contains(&p));
        assert!(need.is_empty());
    }

    #[test]
    fn cell_to_page_coord_outside_image_area_is_none() {
        // image_area at (5,2)..(85,42); cell (4,1) is before x; cell
        // (90,3) is past width; cell (10,50) is past height.
        let layout = layout_for(3, 100);
        let area = Rect {
            x: 5,
            y: 2,
            width: 80,
            height: 40,
        };
        let cell = (10u16, 20u16);
        assert!(
            cell_to_page_coord_pure(4, 10, area, cell, (800, 800), 0.0, 0, &layout, 3).is_none()
        );
        assert!(
            cell_to_page_coord_pure(10, 1, area, cell, (800, 800), 0.0, 0, &layout, 3).is_none()
        );
        assert!(
            cell_to_page_coord_pure(90, 3, area, cell, (800, 800), 0.0, 0, &layout, 3).is_none()
        );
        assert!(
            cell_to_page_coord_pure(10, 50, area, cell, (800, 800), 0.0, 0, &layout, 3).is_none()
        );
    }

    #[test]
    fn cell_to_page_coord_centered_page_at_zero_zoom_one() {
        // viewport_w=100, fit_width_px=100 → no centering offset.
        // image area starts at (0,0) for clean math; cell_size 1×1 px.
        let layout = layout_for(3, 100); // page 0 height = 200
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 200,
        };
        let cell_size = (1u16, 1u16);
        let viewport = (100u32, 200u32);

        // Click at (50, 100) → page 0, nx=0.5, ny=0.5.
        let r = cell_to_page_coord_pure(50, 100, area, cell_size, viewport, 0.0, 0, &layout, 3);
        let (p, nx, ny) = r.expect("center hit must land on page 0");
        assert_eq!(p, 0);
        assert!((nx - 0.5).abs() < 1e-3);
        assert!((ny - 0.5).abs() < 1e-3);
    }

    #[test]
    fn cell_to_page_coord_inside_inter_page_gap_is_none() {
        // Page 0 ends at y=200; gap [200,208); page 1 starts at 208.
        let layout = layout_for(3, 100);
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 250,
        };
        let r = cell_to_page_coord_pure(50, 204, area, (1, 1), (100, 250), 0.0, 0, &layout, 3);
        assert!(r.is_none(), "click in gap should be None, got {r:?}");
    }

    #[test]
    fn cell_to_page_coord_past_end_is_none() {
        // 3 pages, scrolled past total — click at top of viewport
        // is past doc end.
        let layout = layout_for(3, 100);
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 50,
        };
        // Total = 200*3 + 8*2 = 616. Scroll well past it.
        let r = cell_to_page_coord_pure(50, 0, area, (1, 1), (100, 50), 0.0, 9999, &layout, 3);
        assert!(r.is_none(), "past-end click should be None");
    }

    #[test]
    fn cell_to_page_coord_horizontal_centering_when_zoomed_out() {
        // fit_width = 50, viewport_w = 100 → page strip centered with
        // 25px of bg on each side. A click at viewport_x=10 sits in
        // the left bg → None. Click at viewport_x=50 (center) → nx=0.5.
        let layout = layout_for(3, 50);
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        let viewport = (100u32, 100u32);
        let cell = (1u16, 1u16);

        let r_bg = cell_to_page_coord_pure(10, 10, area, cell, viewport, 0.0, 0, &layout, 3);
        assert!(r_bg.is_none(), "click in centering bg must be None");

        let r = cell_to_page_coord_pure(50, 10, area, cell, viewport, 0.0, 0, &layout, 3);
        let (_, nx, _) = r.expect("center click must hit page 0");
        assert!((nx - 0.5).abs() < 1e-2, "nx was {nx}");
    }

    #[test]
    fn cell_to_page_coord_horizontal_scroll_when_zoomed_in() {
        // fit_width=200, viewport_w=100 → 100px of horizontal
        // overflow. scroll_x=0.5 → page strip starts at viewport_x=-50.
        // Click at viewport_x=50 → page_x = 50 - (-50) = 100; fw=200
        // → nx = 0.5.
        let layout = layout_for(3, 200);
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 200,
        };
        let viewport = (100u32, 200u32);
        let cell = (1u16, 1u16);

        let r = cell_to_page_coord_pure(50, 100, area, cell, viewport, 0.5, 0, &layout, 3);
        let (_, nx, _) = r.expect("center click must hit page 0");
        assert!((nx - 0.5).abs() < 1e-2, "nx was {nx}");
    }

    #[test]
    fn nudge_rect_slides_within_bounds() {
        let sel = Rect01 {
            x: 0.4,
            y: 0.4,
            w: 0.2,
            h: 0.2,
        };
        let r = nudge_rect(sel, 0.1, 0.0, false);
        assert!((r.x - 0.5).abs() < 1e-4);
        assert_eq!(r.w, 0.2);
    }

    #[test]
    fn nudge_rect_clamps_when_sliding_off_edge() {
        let sel = Rect01 {
            x: 0.9,
            y: 0.0,
            w: 0.2,
            h: 0.2,
        };
        let r = nudge_rect(sel, 0.5, 0.0, false);
        assert!((r.x - 0.8).abs() < 1e-4, "x was {}", r.x);
    }

    #[test]
    fn nudge_rect_resize_grows_from_bottom_right() {
        let sel = Rect01 {
            x: 0.1,
            y: 0.1,
            w: 0.2,
            h: 0.2,
        };
        let r = nudge_rect(sel, 0.05, -0.05, true);
        assert_eq!((r.x, r.y), (0.1, 0.1));
        assert!((r.w - 0.25).abs() < 1e-4);
        assert!((r.h - 0.15).abs() < 1e-4);
    }

    #[test]
    fn nudge_rect_resize_floors_at_one_percent() {
        let mut sel = Rect01 {
            x: 0.1,
            y: 0.1,
            w: 0.2,
            h: 0.2,
        };
        for _ in 0..50 {
            sel = nudge_rect(sel, -0.1, -0.1, true);
        }
        assert!((sel.w - 0.01).abs() < 1e-4);
        assert!((sel.h - 0.01).abs() < 1e-4);
    }

    /// Pure version of `App::note_input` + `App::is_rapid_scrolling`
    /// for unit testing. Returns the new (last_input_at, burst_count,
    /// is_rapid) given the current state and the time of the new event.
    /// Uses the real module-level constants so a regression in one
    /// fails the other.
    fn step_burst(
        prev_at: Option<std::time::Instant>,
        prev_count: u32,
        now: std::time::Instant,
    ) -> (std::time::Instant, u32, bool) {
        let in_window = prev_at
            .map(|t| (now - t).as_millis() < super::RAPID_SCROLL_THRESHOLD_MS)
            .unwrap_or(false);
        let count = if in_window {
            prev_count.saturating_add(1)
        } else {
            1
        };
        let rapid = count >= super::RAPID_SCROLL_BURST_MIN;
        (now, count, rapid)
    }

    #[test]
    fn burst_single_input_is_not_rapid() {
        let now = std::time::Instant::now();
        let (_, count, rapid) = step_burst(None, 0, now);
        assert_eq!(count, 1);
        assert!(
            !rapid,
            "first event after a long pause must not defer the page"
        );
    }

    #[test]
    fn burst_three_inputs_within_window_is_rapid() {
        let t0 = std::time::Instant::now();
        let (a, c, _) = step_burst(None, 0, t0);
        let (a, c, r) = step_burst(Some(a), c, t0 + std::time::Duration::from_millis(40));
        assert_eq!(c, 2);
        assert!(!r, "two events not enough — burst threshold is 3");
        let (_, c, r) = step_burst(Some(a), c, t0 + std::time::Duration::from_millis(80));
        assert_eq!(c, 3);
        assert!(r, "three consecutive in-window events trip the burst flag");
    }

    #[test]
    fn burst_clear_resets_count_so_next_event_is_not_rapid() {
        // The settle-redraw branch in main::run_loop calls
        // App::clear_input_burst when the poll wait times out, so the
        // catch-up draw sees is_rapid_scrolling = false and transmits
        // any deferred cold pages. Without this reset, SETTLE_MS (120
        // ms) < RAPID_SCROLL_THRESHOLD_MS (250 ms) means the catch-up
        // draw fires while the burst is still considered active, the
        // cold page gets deferred again, and the user sees a blank
        // current page until 130 ms later.
        let t0 = std::time::Instant::now();
        let mut state = (None, 0u32);
        for i in 0..5 {
            let (a, c, _) = step_burst(
                state.0,
                state.1,
                t0 + std::time::Duration::from_millis(i * 30),
            );
            state = (Some(a), c);
        }
        assert!(
            state.1 >= super::RAPID_SCROLL_BURST_MIN,
            "precondition: we should be in burst mode after 5 fast events"
        );
        // Simulate App::clear_input_burst — the count resets but
        // last_input_at stays put (no time has passed).
        let cleared_count = 0u32;
        // The catch-up draw runs immediately after; rapid check uses
        // both `recent` AND `count >= MIN`. With count=0, rapid=false
        // regardless of recency.
        let rapid_after_clear = cleared_count >= super::RAPID_SCROLL_BURST_MIN;
        assert!(
            !rapid_after_clear,
            "clear_input_burst must drop is_rapid_scrolling"
        );
        // The next real event arrives — it must rebuild the count
        // from 1, NOT inherit the cleared zero as "fresh".
        let (_, c_after_event, r_after_event) = step_burst(
            state.0,
            cleared_count,
            t0 + std::time::Duration::from_millis(150),
        );
        assert_eq!(
            c_after_event, 1,
            "after clear, the next event re-arms count from 1"
        );
        assert!(
            !r_after_event,
            "single-event count is below the burst threshold"
        );
    }

    #[test]
    fn burst_resets_after_long_pause() {
        let t0 = std::time::Instant::now();
        // Build up a burst of 5.
        let mut state = (None, 0u32);
        for i in 0..5 {
            let (a, c, _) = step_burst(
                state.0,
                state.1,
                t0 + std::time::Duration::from_millis(i * 30),
            );
            state = (Some(a), c);
        }
        assert_eq!(state.1, 5);
        // Now a long pause.
        let (_, c, r) = step_burst(state.0, state.1, t0 + std::time::Duration::from_millis(500));
        assert_eq!(c, 1, "out-of-window event must reset the count");
        assert!(!r);
    }

    // ---- scroll-keypress throttle ---------------------------------
    //
    // Pure version of `App::note_scroll_attempt` for unit testing.
    // Returns the new (last_applied_at, allow) given the current
    // state and a candidate keypress time. Mirrors the real method's
    // behaviour: an attempt is allowed if the previous applied scroll
    // is at least SCROLL_THROTTLE_MS old (or there's no previous one).
    // Disallowed attempts must NOT advance last_applied_at —
    // otherwise a stream of held-key events at <THROTTLE rate would
    // each push the timer forward and starve the throttle indefinitely.
    fn step_throttle(
        prev_applied_at: Option<std::time::Instant>,
        now: std::time::Instant,
    ) -> (Option<std::time::Instant>, bool) {
        let allow =
            prev_applied_at.is_none_or(|t| (now - t).as_millis() >= super::SCROLL_THROTTLE_MS);
        let new_at = if allow { Some(now) } else { prev_applied_at };
        (new_at, allow)
    }

    #[test]
    fn throttle_first_attempt_always_allowed() {
        let now = std::time::Instant::now();
        let (state, allow) = step_throttle(None, now);
        assert!(
            allow,
            "first scroll after a long pause must always go through"
        );
        assert_eq!(state, Some(now), "allowed attempt records the time");
    }

    #[test]
    fn throttle_second_attempt_within_window_dropped() {
        let t0 = std::time::Instant::now();
        let (state, _) = step_throttle(None, t0);
        // 30 Hz autorepeat = ~33 ms; well inside the 150 ms throttle.
        let t1 = t0 + std::time::Duration::from_millis(33);
        let (state2, allow) = step_throttle(state, t1);
        assert!(!allow, "second attempt within SCROLL_THROTTLE_MS must drop");
        assert_eq!(
            state2, state,
            "dropped attempt must NOT advance last_applied_at — otherwise a held key starves the throttle"
        );
    }

    #[test]
    fn throttle_attempt_after_window_allowed() {
        let t0 = std::time::Instant::now();
        let (state, _) = step_throttle(None, t0);
        let t1 = t0 + std::time::Duration::from_millis(super::SCROLL_THROTTLE_MS as u64);
        let (state2, allow) = step_throttle(state, t1);
        assert!(
            allow,
            "attempt at exactly SCROLL_THROTTLE_MS must be allowed"
        );
        assert_eq!(state2, Some(t1), "allowed attempt records the new time");
    }

    #[test]
    fn throttle_held_key_30hz_caps_at_67hz() {
        // Simulate Linux keyboard autorepeat: 30 Hz = one event every 33 ms.
        // Over 1 second (30 events) we expect at most ceil(1000 / 150) = 7
        // accepted scrolls — i.e. ~6.7 Hz, the throttle's design rate.
        let t0 = std::time::Instant::now();
        let mut state: Option<std::time::Instant> = None;
        let mut accepted = 0u32;
        for i in 0..30 {
            let (new_state, allow) =
                step_throttle(state, t0 + std::time::Duration::from_millis(i * 33));
            state = new_state;
            if allow {
                accepted += 1;
            }
        }
        assert!(
            (6..=7).contains(&accepted),
            "30 Hz held-key over 1s should pass {{6,7}} scrolls; got {accepted}"
        );
    }

    // ---- link-hint state machine ----------------------------------
    //
    // Tests the pure transition logic without spinning up an App or
    // pdfium. We synthesise hint entries directly and call
    // hint_keystroke through a small helper that mimics what
    // `enter_link_hint_mode` would do.

    fn hint_step(
        hints: &[HintEntry],
        filter_so_far: &str,
        c: char,
    ) -> (String, Option<LinkAction>, bool) {
        // Simulate the same state-machine logic as App::hint_keystroke
        // but without a full App. Returns (new filter, action if
        // disambiguated, "exit hint mode" flag).
        let mut filter = filter_so_far.to_string();
        filter.push(c);
        let matches: Vec<&HintEntry> = hints
            .iter()
            .filter(|e| e.label.starts_with(&filter))
            .collect();
        match matches.len() {
            0 => (filter, None, true),
            1 => (filter, Some(matches[0].action.clone()), true),
            _ => (filter, None, false),
        }
    }

    fn fake_hint(label: &str, action: LinkAction) -> HintEntry {
        HintEntry {
            page_idx: 0,
            rect: Rect01 {
                x: 0.0,
                y: 0.0,
                w: 0.1,
                h: 0.1,
            },
            action,
            label: label.into(),
        }
    }

    #[test]
    fn hint_unique_match_dispatches_action() {
        let hints = vec![
            fake_hint("a", LinkAction::GoToPage(5)),
            fake_hint("bc", LinkAction::Url("https://example.com".into())),
            fake_hint("bd", LinkAction::GoToPage(7)),
        ];
        // 'a' is uniquely matching → action fires immediately.
        let (filter, action, exit) = hint_step(&hints, "", 'a');
        assert_eq!(filter, "a");
        assert!(matches!(action, Some(LinkAction::GoToPage(5))));
        assert!(exit, "unique match should exit hint mode");
    }

    #[test]
    fn hint_ambiguous_keystroke_narrows_without_dispatching() {
        let hints = vec![
            fake_hint("aa", LinkAction::GoToPage(1)),
            fake_hint("ab", LinkAction::GoToPage(2)),
            fake_hint("c", LinkAction::GoToPage(3)),
        ];
        // 'a' matches both "aa" and "ab" — should narrow but not fire.
        let (filter, action, exit) = hint_step(&hints, "", 'a');
        assert_eq!(filter, "a");
        assert!(action.is_none(), "ambiguous match must not fire");
        assert!(!exit, "ambiguous match must stay in hint mode");
        // Second char "a" → uniquely "aa".
        let (filter2, action2, exit2) = hint_step(&hints, &filter, 'a');
        assert_eq!(filter2, "aa");
        assert!(matches!(action2, Some(LinkAction::GoToPage(1))));
        assert!(exit2);
    }

    // ---- section jump --------------------------------------------

    #[test]
    fn next_section_forward_picks_next_outline_page() {
        let outline = vec![0, 5, 12, 20];
        // Mid-section page 7 → next is 12.
        assert_eq!(next_section_target(&outline, 7, 1), Some(12));
        // On a section start (5) → next is 12.
        assert_eq!(next_section_target(&outline, 5, 1), Some(12));
        // Past last section → None.
        assert_eq!(next_section_target(&outline, 25, 1), None);
    }

    #[test]
    fn next_section_back_picks_previous_outline_page() {
        let outline = vec![0, 5, 12, 20];
        // Mid-section page 15 → previous is 12 (start of current).
        assert_eq!(next_section_target(&outline, 15, -1), Some(12));
        // On a section start (12) → previous is 5.
        assert_eq!(next_section_target(&outline, 12, -1), Some(5));
        // Before first section → None.
        assert_eq!(next_section_target(&outline, 0, -1), None);
    }

    // ---- find_references_page ------------------------------------

    fn outline_entry(title: &str, page: Option<usize>) -> OutlineEntry {
        OutlineEntry {
            title: title.to_string(),
            lc_title: title.to_lowercase().chars().collect(),
            depth: 0,
            page,
        }
    }

    #[test]
    fn find_references_matches_canonical_titles() {
        let outline = vec![
            outline_entry("Chapter 1", Some(0)),
            outline_entry("References", Some(45)),
        ];
        assert_eq!(find_references_page(&outline), Some(45));
    }

    #[test]
    fn find_references_matches_substring() {
        let outline = vec![
            outline_entry("Selected Bibliography", Some(98)),
            outline_entry("Index", Some(102)),
        ];
        assert_eq!(find_references_page(&outline), Some(98));
    }

    #[test]
    fn find_references_is_case_insensitive() {
        let outline = vec![outline_entry("WORKS CITED", Some(7))];
        assert_eq!(find_references_page(&outline), Some(7));
    }

    #[test]
    fn find_references_skips_unresolved_pages() {
        let outline = vec![
            outline_entry("References", None),
            outline_entry("Index", Some(50)),
        ];
        // No resolved page on References → returns None (next entry
        // doesn't match the keyword).
        assert_eq!(find_references_page(&outline), None);
    }

    #[test]
    fn find_references_returns_none_for_missing() {
        let outline = vec![
            outline_entry("Chapter 1", Some(0)),
            outline_entry("Chapter 2", Some(20)),
        ];
        assert_eq!(find_references_page(&outline), None);
    }

    #[test]
    fn hint_no_match_exits_without_dispatch() {
        let hints = vec![
            fake_hint("a", LinkAction::GoToPage(0)),
            fake_hint("b", LinkAction::GoToPage(1)),
        ];
        // 'x' matches nothing → exit, no action.
        let (filter, action, exit) = hint_step(&hints, "", 'x');
        assert_eq!(filter, "x");
        assert!(action.is_none(), "no-match must not dispatch");
        assert!(exit, "no-match must exit hint mode");
    }

    #[test]
    fn human_bytes_picks_largest_unit() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(1500), "1.5 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(
            human_bytes(1024u64 * 1024 * 1024 * 3 + 1024 * 1024 * 512),
            "3.5 GB"
        );
    }
}
