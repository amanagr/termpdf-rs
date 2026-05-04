<div align="center">

# termpdf-rs

**A power-efficient PDF reader that lives inside your terminal — vim
keys, kitty-native rendering, near-zero CPU at idle.**

Pages render as actual images via the
[Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/) —
text stays sharp, figures stay readable, no halfblocks-ASCII guesswork.

`vim`-keys · per-PDF session · color-aware dark mode · indexed search ·
link-follow · highlights stored *in* the PDF · idle redraws gated so
your laptop doesn't heat up while reading

</div>

---

```
┌──────────────────────────────────────────────────────────────────┐
│                                                                  │
│       [a real, pixel-perfect PDF page rendered as an image]      │
│                                                                  │
│                                                                  │
│                                                                  │
└──────────────────────────────────────────────────────────────────┘
   12/300  zoom 100%  DARK              [j]  found 7 matches
```

## Why this exists

Most terminal PDF "readers" either (a) draw halfblocks and pretend
that's reading, or (b) use real graphics protocols but burn CPU as if
you were watching video — a held-`j` on a 600-page book pegs a core,
the laptop discharges while plugged in, the fan kicks on.

termpdf-rs is the inverse: real pixel-perfect images, but the
scroll-keystroke and idle paths are budgeted ruthlessly so the
reading experience stays cool. Idle with a PDF open emits effectively
no bytes on the pty (gated behind a dirty flag); a sustained scroll
burst on a 600-page book lands in single-digit-percent CPU on the
client side.

## What you actually get

- **Open any PDF, scroll smoothly with `j`/`k`/Space.** Per-page
  kitty image IDs + idle pre-transmit + on-disk PNG cache mean a held
  `j` runs without visible lag once the prefetch tier warms up.
- **Power-efficient by design.** Idle term.draw is gated on a `dirty`
  flag — when the user does nothing, the binary writes nothing to the
  pty. Held-key bursts defer cold renders entirely; a single settle
  redraw catches up when input goes idle. Hold `j` and CPU stays in
  single digits where a browser sits at 60+ %.
- **Indexed full-text search.** First search builds a back-index in
  the background; subsequent searches are
  [Sioyek-fast](https://ahrm.github.io/jekyll/update/2022/09/11/pdf-viewer-text-search-benchmark.html):
  a query matching 5 of 700 pages does 5 pdfium scans, not 700.
  The index persists to disk between sessions.
- **Vim text-objects on PDF text.** `viw` / `vis` / `vip` work. Yank
  as plain text (`Y`), save as a highlight (`y`), or `gy` for a
  Markdown blockquote with a `— file.pdf, p. 12` citation footer
  ready to paste into your notes.
- **Highlights live in the PDF itself**, not a sidecar JSON. They
  travel with the file, render in Adobe / Preview / Sioyek, and
  survive moves and renames.
- **Color-aware dark mode.** Luminance-only HSL inversion — red text
  stays red, blue charts stay blue. The "every dark-mode tool turns
  red into cyan" bug is one channel-flip you don't have to deal with.
- **Vimium-style link follow.** Press `f` → 1-2 char hints overlay
  every clickable link on visible pages. Type the hint to jump
  (internal) or `xdg-open` (URLs).
- **Section jump with `]]` / `[[`.** Walks the document outline so
  navigating tech books / RFCs feels like vim navigating a header
  tree.

## Try it in 60 seconds

```sh
git clone https://github.com/amanagr/termpdf-rs && cd termpdf-rs
./setup.sh                        # vendors libpdfium.so (~7.5 MB)
cargo build --release
./target/release/termpdf paper.pdf
```

A shell alias lands you in a productive state immediately:

```sh
alias pdf='~/termpdf-rs/target/release/termpdf'
```

Then `pdf paper.pdf` from anywhere. **Press `?` at any time** to see
every keybinding in a help overlay.

## Five keys to get started

| Key      | What it does                                            |
| -------- | ------------------------------------------------------- |
| `j` / `k`| Next / prev page                                        |
| Space    | Scroll one screen down (less-style)                     |
| `o`      | Open the table-of-contents panel — `/` to fuzzy-filter  |
| `v`      | Visual mode → `viw`/`vis`/`vip` → `y` to highlight      |
| `f`      | Link-follow — type the hint over any clickable link     |

That's enough to read with. Everything else builds on this.

## Daily-use cheat sheet

The full keymap is in the `?` overlay; this is the high-leverage subset.

### Navigate

| Keys                       | Action                                    |
| -------------------------- | ----------------------------------------- |
| `j` / `k`                  | next / prev page                          |
| Space / `b`                | one screen down / up (less-style)         |
| `Ctrl-d` / `Ctrl-u`        | half-screen down / up                     |
| `gg` / `G`                 | first / last page                         |
| `:<n>` / `N G`             | jump to page N                            |
| `]]` / `[[`                | next / prev outline section               |
| `m{a-z}` / `'{a-z}`        | set / jump to mark (persisted per PDF)    |
| `Ctrl-o` / `Ctrl-i`        | jumplist back / forward                   |
| `+` / `-` / `0`            | zoom in / out / reset                     |
| `d`                        | toggle color-aware dark mode              |
| `f`                        | link-follow hint mode                     |

### Highlight & quote (Visual — `v`)

| Keys                       | Action                                    |
| -------------------------- | ----------------------------------------- |
| `h j k l` `w b e` `0 ^ $`  | move the caret like in vim                |
| `iw` / `is` / `ip`         | select inner word / sentence / paragraph  |
| `V` / `Ctrl-v`             | linewise / blockwise selection            |
| `c`                        | cycle highlight color                     |
| `y`                        | save highlight + copy plain text          |
| `Y`                        | copy plain text only (no highlight)       |
| `gy`                       | copy as Markdown blockquote with citation |
| click + drag               | highlight with the mouse                  |
| `x` (Normal)               | delete last highlight on current page     |

### Search, TOC, export

| Keys                       | Action                                    |
| -------------------------- | ----------------------------------------- |
| `/<query>` then `n` / `N`  | search · next / prev match (indexed)      |
| `:nohl`                    | clear search results                      |
| `o`                        | open TOC panel — `/` to filter, Enter jumps |
| `:export [path]`           | dump highlights as a Markdown notes file  |

## Power efficiency

This is the headline. PDF reading is a low-frequency activity by
nature — pages turn at human speed, not video speed — so a reader
that pegs CPU during scrolling is wasting your battery on nothing.

### vs. opening the same PDF in a browser

The most common alternative for "I just want to read this PDF" is
double-clicking it and letting Chrome / Firefox open it. Browsers
ship the full web-platform stack to do that — V8, Blink/Gecko, GPU
compositor, IPC layers, multi-process accounting — for a job that
needs none of it. termpdf-rs uses pdfium (the same PDF engine
Chrome ships) without any of the surrounding browser machinery.

There's a benchmark script in the repo that measures both on your
machine: `scripts/bench-vs-browser.sh path/to/file.pdf`. It opens
the same PDF in termpdf-rs and in your default browser, waits 10 s
for warmup to settle, then samples CPU% and RSS for 20 s of
steady-state idle.

Steady-state idle on a 600-page PDF (Designing Data-Intensive
Applications), measured with `bench-vs-browser.sh`:

| Metric    | termpdf-rs | google-chrome | ratio |
| --------- | ---------- | ------------- | ----- |
| Idle CPU% | 3.2%       | 17.1%         | 5.3×  |
| Idle RSS  | 71 MB      | 558 MB        | 7.9×  |

For interactive scroll, use `monitor-scroll.sh` and scroll manually
in each app — it delta-samples `/proc/<pid>/stat` once per second so
the value reflects current CPU usage (the same thing `top` reports),
not the lifetime average that `ps -o pcpu` gives. On the same DDIA
book, sustained held-`j` / Page Down for ~25 s:

| Metric                    | termpdf-rs + Ghostty | Firefox     |
| ------------------------- | -------------------- | ----------- |
| Scroll CPU% (median, 25s) | 7.5%                 | 66.0%       |
| Scroll CPU% (max)         | 8.0%                 | 74.0%       |

Almost 9× lower CPU during active scroll, on the same machine and
the same PDF. The 7.5 % we measure is the *combined* termpdf-rs +
Ghostty cost — termpdf is roughly 1-2 % on its own; the bulk is
Ghostty PNG-decoding the page bitmaps and uploading textures. Set
`TERMPDF_TRANSMIT_RAW=1` to ship raw RGBA instead of PNG (skips
Ghostty's decode at the cost of larger pty bytes) and see whether
your terminal prefers less compute or less bandwidth.

The aim isn't to beat the absolute ceiling — it's to make the case
that for a "just open this PDF" session, you don't need the web
platform's overhead. Run the script yourself for the actual idle
numbers on your hardware — GPU support, browser version, and PDF
size all move the result.

### What gets us there

Concrete moves that get us to single-digit CPU during reading:

- **Idle redraws gated on a dirty flag.** `run_loop` only calls
  `term.draw` when something has actually changed since the last
  paint (input dispatched, settle catch-up, cold-redraw catch-up).
  Without this, the loop poked ratatui every 250 ms (idle poll
  cadence), and even with a small diff the per-frame backend flush
  was enough to keep the terminal's pty reader awake. Result: bytes
  written to the pty when the user is doing nothing → effectively
  zero.
- **Held-key burst defer.** Within ~250 ms of consecutive input, cold
  pdfium renders are skipped entirely. The settle redraw fires once
  the burst ends and renders the final page. A held-`j` on a 600-page
  book burns roughly the cost of one render, not 600.
- **LCD subpixel rendering off on the Fast tier.** Saves ~40-60 % of
  the per-page pdfium time during scroll. The Sharp tier (idle
  background upgrade) keeps LCD on so the visible page sharpens
  shortly after the scroll settles. Toggle with `TERMPDF_FAST_LCD=1`
  if you'd rather pay the heat for first-frame sharpness.
- **Per-page kitty placement** — each page becomes its own kitty
  image with a stable ID. Scrolling re-uses the in-terminal bitmap;
  only placement cells go on the wire (a few hundred bytes per
  visible page per scroll, vs. multi-MB canvas re-encodes).
- **Layered selection overlay.** Visual-mode selection ships as a
  separate, mostly-transparent kitty image at z=1 above the page.
  Dragging the selection re-encodes a few-KB overlay PNG, not the
  full multi-MB page bitmap.
- **Tiered cache** — pdfium RGBA in `page_cache`, post-overlay PNG
  in the kitty registry, encoded payload bytes ride along, all three
  tiers checked per draw.
- **LRU eviction** with `a=d,d=I,i=ID` deletes so a 700-page sweep
  doesn't pile gigabytes of decoded RGBA in your terminal.
- **Idle warm** — between draws, render + encode + pre-transmit
  upcoming pages so the next keypress lands on a warm cache.
- **Disk cache** at `~/.cache/termpdf-rs/<file-hash>/<page>.png`
  bounded to 512 MB; re-opens skip pdfium entirely.
- **Persistent search index** at the same hash key; reopens skip the
  ~3.5 s text-extract cost.

## Reading PDFs lives nicely with…

```sh
pdf paper.pdf            # in one tmux pane
nvim notes.md            # in another
# In Visual mode: `gy` copies a Markdown quote with citation.
# Paste into nvim. Done.
```

Highlights are written into the PDF on quit, so syncing or sharing
the file carries your annotations. To dump everything to Markdown
for Obsidian / Zettelkasten:

```
:export ~/notes/paper.notes.md
```

## Requirements

- **OS:** Linux x86\_64 (the bundled `setup.sh` only fetches a pinned
  pdfium build for that target).
- **Terminal, ranked best-to-worst:**
  - **Kitty / Ghostty / WezTerm** — first-class, pixel-perfect.
  - **xterm / foot / Konsole** — pass `--protocol sixel`.
  - Anything else → halfblocks fallback. Text will be illegible;
    use this only to confirm it loads.
- **Password-protected PDFs are not yet supported.** Decrypt first:
  ```sh
  qpdf --decrypt locked.pdf unlocked.pdf
  ```

## Troubleshooting

**"Images don't render in tmux."** Run once:
```sh
tmux set -g allow-passthrough on
```
The reader prints a one-time hint. After it shows, an ack-marker
under `$XDG_DATA_HOME/termpdf-rs/.tmux-hint-acked` suppresses it.
Delete that file to re-arm.

**"Nothing renders, just garbage characters."** Your terminal didn't
advertise a graphics protocol. Try `--protocol sixel` or move to
Kitty/Ghostty/WezTerm.

**"My highlights disappeared."** termpdf-rs writes to the PDF on
clean exit only. If you killed the process with `kill -9`, the
in-memory store wasn't flushed. Plain `q` / `:q` / Ctrl-C all save.

**"The first search on a big book is slow."** The full-text index
fills in the background — first search at indexing X% will fall
back to per-page pdfium for the unindexed pages. Status line shows
the percent. Once 100%, the index persists; subsequent opens are
instant.

**"My terminal is using a lot of CPU."** Run
`scripts/monitor-scroll.sh termpdf <your-terminal>` while scrolling.
If termpdf's own CPU is high, the regression is on our side and we'd
love a bug report. If termpdf is low but the terminal is high during
scroll, the cost is downstream PNG decode + GPU upload — try
`TERMPDF_TRANSMIT_RAW=1` to ship raw RGBA instead of PNG (skips
decode in the terminal at the cost of larger pty bytes).

## Configuration

Almost none — termpdf-rs reads a handful of environment variables and
a session file. By design.

| Var                       | Default                            | Meaning                                  |
| ------------------------- | ---------------------------------- | ---------------------------------------- |
| `TERMPDF_PDFIUM`          | `vendor/libpdfium.so` next to bin  | Override path to libpdfium.so            |
| `TERMPDF_PROTOCOL`        | `auto`                             | Force `kitty`/`sixel`/`iterm2`/`halfblocks` |
| `TERMPDF_NO_TMUX_HINT`    | unset                              | Skip the one-time tmux hint              |
| `TERMPDF_CACHE_MB`        | `256`                              | Soft cap on the in-memory page cache     |
| `TERMPDF_PROFILE`         | unset                              | Print phase timings to stderr at exit    |
| `TERMPDF_FAST_LCD`        | unset                              | Re-enable LCD subpixel text on the Fast tier (sharper scroll, more CPU) |
| `TERMPDF_TRANSMIT_RAW`    | unset                              | Ship raw RGBA instead of PNG (skip terminal-side PNG decode at the cost of larger pty bytes) |

Per-PDF state (current page, dark flag, zoom, marks) lives at
`$XDG_DATA_HOME/termpdf-rs/<name>.<hash>.session.json` (mode 0600).
Per-PDF rendered-page cache + search index live at
`$XDG_CACHE_HOME/termpdf-rs/<file-hash>/`. Same hash scheme means
two PDFs with the same name in different directories don't collide.

## Highlights

Stored as native `PdfPageAnnotationType::Highlight` annotations on
the PDF itself. Other readers see a normal yellow rectangle in the
right place; termpdf-rs adds a small JSON tag in the annotation's
`Contents` field so it can recover the exact color name and any
inline note on the next open.

Atomic-write on save: the new PDF is written into a sibling tempfile
with mode 0600 *before* pdfium fills it, then renamed over the
original. A crash mid-write leaves your original PDF untouched.

## Why this stack

- **[pdfium-render](https://crates.io/crates/pdfium-render)** —
  Chromium's PDF engine via a thin Rust wrapper. BSD-3, dynamically
  loaded so this crate doesn't compile pdfium itself.
- **[ratatui](https://ratatui.rs/) + [ratatui-image](https://crates.io/crates/ratatui-image)**
  — TUI plumbing + Kitty/sixel/iTerm2/halfblocks image embedding,
  picked at runtime from what the terminal advertises.
- **[palette](https://crates.io/crates/palette)** — luminance-only
  HSL inversion for dark mode. The "red turns cyan" bug almost every
  PDF dark mode ships is one channel-flip away.

## Limitations / not yet

- Linux x86\_64 only (other targets need a different libpdfium).
- No EPUB / MOBI (would need a swap to MuPDF — pdfium is PDF-only).
- No multi-doc tabs; one PDF per process.
- No password-protected PDFs (decrypt with `qpdf` first).
- No SyncTeX.
- No PDF form fields.
- No two-page (book-spread) layout yet.
- Two-column papers extract scrambled text on yank/`:export` — known
  bug in the line-clustering algorithm; planned fix splits columns
  before Y-banding.

## Contributing

The code is ~12 k LOC of Rust, ~150 unit tests + 3 pty-driven
integration tests. `cargo test --release` on a fresh clone (after
`./setup.sh`) should pass the lot. Tests don't need fixture files —
the pdfium-render crate creates synthetic PDFs in-process; see
`src/pdfhighlights.rs::tests` and `tests/perf_regression.rs` for
patterns.

PRs welcome. Two house rules:

1. **Run the tests** before pushing — this includes the e2e perf
   regression test that catches refactors silently breaking the
   disk-cache / idle-warm wiring AND the new idle-bytes guard
   (asserts the binary writes <4 KB to the pty across 3 s of
   fully-settled idle, the proxy for "we're not heating up the
   user's terminal").
2. **Match the comment style.** Prefer a one-line "why this is
   non-obvious" over an apology for the code; never write what's
   already in the identifier names.
