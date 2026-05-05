# Architecture

This document is for contributors who need to navigate the codebase
and understand the data flow. It is **not** a feature tour — see
README.md for that. It is also not exhaustive — see the doc-comments
inside individual modules for the full story of each subsystem.

## High-level shape

```
                  ┌──────────────────────┐
                  │ pdfium-render        │
                  │ (dlopen libpdfium)   │
                  └──────────┬───────────┘
                             │
                             ▼
                  ┌──────────────────────┐
                  │ pdf::Doc             │  page count, page dims,
                  │ (process-global)     │  link extraction
                  └──────────┬───────────┘
                             │
                             ▼
   ┌────────────┐    ┌──────────────────┐    ┌────────────────┐
   │ disk_cache │◄──►│ App<'doc>        │◄──►│ HighlightStore │
   │ (PNG, LRU) │    │ — current state  │    │ (JSON on disk) │
   └────────────┘    └────────┬─────────┘    └────────────────┘
                              │
                ┌─────────────┴─────────────┐
                │                           │
                ▼                           ▼
        ┌───────────────┐          ┌─────────────────┐
        │ ui::draw      │          │ keys / cmd      │
        │ (per-frame)   │          │ (input handler) │
        └───────┬───────┘          └─────────────────┘
                │
       ┌────────┴────────┐
       │                 │
       ▼                 ▼
┌─────────────┐   ┌──────────────────┐
│ canvas mode │   │ kitty_pages      │
│ (halfblocks │   │ (kitty graphics, │
│ via         │   │ tmux passthrough,│
│ ratatui-img)│   │ image registry,  │
└─────────────┘   │ LRU + deletes)   │
                  └────────┬─────────┘
                           │
                           ▼
                ┌────────────────────┐
                │ pty: stdout writer │
                │ (escape sequences) │
                └────────────────────┘
```

## Crate layout (`src/`)

| Module             | Lines | Responsibility |
|--------------------|------:|----------------|
| `main.rs`          |  1100 | Process entry, event loop, idle policy, panic hook |
| `app.rs`           |  3600 | The `App` struct — every cross-frame piece of state |
| `ui.rs`            |  3100 | `draw()`: ratatui rendering tree + kitty draw path |
| `kitty_pages.rs`   |  2800 | Kitty graphics protocol — registry, transmit, evict |
| `textlayout.rs`    |  1100 | pdfium → text+rects extraction for selection / search |
| `pdfhighlights.rs` |   770 | Reading / writing highlights to PDF /Highlight annots |
| `search_index.rs`  |   600 | Per-PDF text-search index (lazy build, persisted) |
| `keys.rs`          |   570 | Keymap dispatch (normal / visual / link-hint / cmd) |
| `disk_cache.rs`    |   500 | On-disk page-bitmap cache (PNG, LRU-evicted) |
| `search.rs`        |   480 | Search state machine + match navigation |
| `highlight.rs`     |   380 | In-memory `HighlightStore` (per-page rect lists) |
| `compose.rs`       |   280 | RGBA pixel composition primitives |
| `cmd.rs`           |   280 | `:command` mode parser |
| `pdf.rs`           |   270 | Thin wrapper over pdfium-render |
| `links.rs`         |   260 | Link-hint overlay + URL invocation |
| `outline.rs`       |   240 | TOC pane |
| `session.rs`       |   230 | Per-PDF resume state (page, scroll, zoom, etc.) |
| `clipboard.rs`     |   180 | OSC 52 / xclip / wl-copy fallbacks |
| `layout.rs`        |   300 | Per-page layout cache (fit-width, scroll, zoom) |
| `dark.rs`          |    97 | Color-aware bitmap inversion for dark mode |
| `term_safe.rs`     |    93 | Stdout sanitization for status / error text |
| `profile.rs`       |   114 | Optional profiling hooks (env-gated) |
| `render_worker.rs` |    64 | Off-main-thread page rendering |

`app.rs` and `ui.rs` are the load-bearing modules; everything else
is a leaf.

## Key data flow paths

### Page render → on-screen pixels

1. `App::ensure_page_rendered(idx)` — checks `page_cache` (RAM), then
   `disk_cache` (PNG on disk), then commits an off-main-thread render
   via `render_worker`.
2. Render output is an `RgbaImage`. If dark mode is active, the
   pixels go through `dark::invert_color_aware`.
3. Highlights bake in via `highlight::bake_into_page_bitmap` (only
   for pages with active highlights). Selection rects also bake in
   here when in Visual mode.
4. `ui::draw_kitty_pages` plans a `PageBlit` per visible page. Each
   `PageBlit` has cell-grid coordinates (`dst_top_cell`,
   `dst_left_cell`, `width_cells`, `height_cells`) and a freshness
   bit (`need_transmit`).
5. `kitty_pages::build_transmit` encodes the bitmap (default:
   `f=24,o=z` RGB+zlib; `TERMPDF_TRANSMIT_PNG=1` for PNG fallback)
   and emits a kitty APC packet to stdout, optionally tmux-wrapped.
6. A separate kitty placement APC tells the terminal where to draw
   the image. ratatui-image renders unicode placeholder cells
   (U+10EEEE block) that the terminal substitutes with image
   pixels.

### Idle policy (`main.rs::idle_action`)

The event loop tiers idle behavior so that an open-but-untouched
PDF emits effectively zero bytes:

- **Active** (recent input): full per-frame rendering, MIN_FRAME_INTERVAL.
- **Settling** (just finished a burst): one or two more frames to
  let prefetch fill the cache, then transition to idle.
- **Idle** (no input for ~30 s): no draws, no transmits. Single
  60 s heartbeat re-issues all visible page transmits as a
  recovery hatch (Ghostty / tmux occasionally drop cached images).

### Selection → bitmap path (post-bake)

Selection rects historically rendered as a separate transparent
overlay image with classical kitty placement (`a=p,U=0`). This was
ripped out in commit `f83131f` because classical placements bypass
tmux pane clipping and bleed into adjacent panes.

The current path bakes selection rects directly into the page
bitmap before transmit. Each Visual-mode keystroke causes a
re-encode + re-transmit of the affected page, but at typical PDF
sizes this is ~5–15 ms wall, well within the budget.

The overlay *registry* (`KittyPageRegistry::overlays`) still
exists for one purpose: when a selection moves off a page, an
`overlay_drop` queues an `a=d` so the terminal frees the
ex-overlay's image storage. This kept the entry point alive even
after the overlay-build path was removed.

### Kitty image registry — eviction + delete coalescing

`KittyPageRegistry` maintains an LRU bounded by
`image-storage-limit` (320 MB on Ghostty, per surface). Eviction
runs at the end of each kitty draw. Evicted image_ids accumulate
in `pending_deletes: Vec<u32>` and ride out as a kitty `a=d` APC
on the next transmit (so we don't pay for a separate write).

`queue_deletes` collapses contiguous-id runs into `d=R,x=A,y=B`
form to keep the APC small. A subtle bug found by property tests
(`registry_proptests`): if a page is evicted, then re-rendered
*before* the queued delete rides out, the stale image_id can
collide with a different page's transmit. Fixed by scrubbing
matching pending deletes inside `mark_transmitted`.

## Tests (`tests/`)

| File                       | What it asserts |
|----------------------------|-----------------|
| `render_kitty.rs`          | Built `termpdf` binary, given `--protocol kitty`, emits at least one transmit + at least one placeholder cell. |
| `render_pty.rs`            | Built binary inside a `portable-pty`, drives keystrokes, asserts on the exact escape-sequence stream. |
| `perf_harness.rs`          | 12 scenarios × 3 runs each, compared against `tests/perf_baseline.json`. **Not run in CI** — wall-clock baselines don't survive cross-machine. |
| `stress_long_session.rs`   | 30-min default soak, asserts `peak_rss/initial_rss < 2.0` and `transmits/sec < 50`. Gated behind `TERMPDF_STRESS_RUN=1`, run nightly via `stress-nightly.yml`. |
| `perf_regression.rs`       | Older smoke test, mostly superseded by `perf_harness`. |

Inside `src/main.rs`, the `kitty_pages::registry_proptests`
module runs property tests on the registry state machine — five
invariants checked across 64 random op sequences (256 in CI).

## Build system

`setup.sh` is the only non-cargo step. It fetches a prebuilt
`libpdfium.so` from `bblanchon/pdfium-binaries` into `vendor/`.
That directory is gitignored; the binary is dlopened at runtime
by `pdfium-render`.

GitHub Actions caches `vendor/` keyed on `setup.sh`'s hash, so
the ~7.5 MB fetch happens once per setup-script change.

## Where to start when contributing

- **Bug in scrolling / blank pages / image storage:** start in
  `kitty_pages.rs` (the registry) and `ui.rs::draw_kitty_pages`
  (the per-frame plan). The post-`f83131f` selection-bake path is
  the recent reshuffle to be aware of.
- **Bug in selection / highlights / search:** `textlayout.rs`
  extracts the rects pdfium gives us; `highlight.rs` /
  `pdfhighlights.rs` persist them; `app.rs::selection_*` drives
  Visual mode.
- **Power / latency optimization:** `main.rs::idle_action` and
  `ui.rs::ColdRenderDecision`. Capture before/after with
  `cargo test --release --test perf_harness` on your local
  baseline.
- **New keybind / mode:** `keys.rs` is the dispatch table.
  Don't add a new keymap module unless the new mode is large
  enough to warrant a separate file (10+ keys).
