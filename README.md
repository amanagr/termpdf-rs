<div align="center">

# termpdf-rs

**A vim-style PDF reader that lives inside your terminal.**

Pages render as actual images via the
[Kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/) —
text stays sharp, figures stay readable, no halfblocks-ASCII guesswork.

`vim`-keys · per-PDF session · color-aware dark mode · indexed search ·
link-follow · highlights stored *in* the PDF · 4 KB of code touches the
hot path on every keystroke

</div>

---

```
┌──────────────────────────────────────────────────────────────────┐
│                                                                  │
│       [a real, pixel-perfect PDF page rendered as an image]      │
│                                                                  │
│    ┌─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─    │
│    │      ↑ 3-second "you were reading here" line drops          │
│    │        when you Space-scroll past where you stopped         │
│                                                                  │
└──────────────────────────────────────────────────────────────────┘
   12/300  zoom 100%  DARK              [j]  found 7 matches    ?
```

## What you actually get

- **Open any PDF, scroll smoothly with `j`/`k`/Space.** Per-page kitty
  image IDs + idle pre-transmit + on-disk PNG cache mean the bandwidth
  budget supports holding `j` at terminal autorepeat without
  visible lag once the prefetch tier warms up.
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
| Space    | Scroll one screen (drops a 3 s line at where you were)  |
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

## Performance

A lot of the work in this repo is on the perf path. The headline
moves between two configs are:

| Operation                              | Cold     | Warm    |
| -------------------------------------- | -------- | ------- |
| Open a PDF (700-page book)             | ~150 ms  | ~150 ms |
| Render the next visible page (`j`)     | ~22 ms   | ~1 ms   |
| Search for a substring                 | per-page | indexed |
| Reopen the same PDF                    | ~150 ms  | ~50 ms (PNG cache hit) |

What gets us there:

- **Per-page kitty placement** — each page becomes its own kitty image
  with a stable ID. Scrolling re-uses the in-terminal bitmap; only
  placement cells go on the wire.
- **Tiered cache** — pdfium RGBA in `page_cache`, post-overlay PNG
  in the kitty registry, encoded payload bytes ride along, all
  three tiers checked per draw.
- **LRU eviction** with `a=d,d=I,i=ID` deletes so a 700-page sweep
  doesn't pile gigabytes of decoded RGBA in your terminal.
- **Idle warm** — between draws, render + encode + pre-transmit up
  to 4 upcoming pages so the next keypress lands on a warm cache.
- **Burst-defer** — held-`j` skips cold transmits during the burst
  and catches up in one settle redraw when the key releases.
- **Disk cache** at `~/.cache/termpdf-rs/<file-hash>/<page>.png`
  bounded to 512 MB; re-opens skip pdfium entirely.
- **Persistent search index** at the same hash key; reopens skip
  the ~3.5 s text-extract cost.

Set `TERMPDF_PROFILE=1` to print phase timings to stderr at exit
(EnsureRendered / EnsureOverlay / Compose / BuildProtocol / Draw /
IdleWarm).

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

## Configuration

Almost none — termpdf-rs reads four environment variables and a
session file. By design.

| Var                       | Default                            | Meaning                                  |
| ------------------------- | ---------------------------------- | ---------------------------------------- |
| `TERMPDF_PDFIUM`          | `vendor/libpdfium.so` next to bin  | Override path to libpdfium.so            |
| `TERMPDF_PROTOCOL`        | `auto`                             | Force `kitty`/`sixel`/`iterm2`/`halfblocks` |
| `TERMPDF_NO_TMUX_HINT`    | unset                              | Skip the one-time tmux hint              |
| `TERMPDF_CACHE_MB`        | `256`                              | Soft cap on the in-memory page cache     |
| `TERMPDF_PROFILE`         | unset                              | Print phase timings to stderr at exit    |

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

The code is ~12 k LOC of Rust, ~160 unit tests + 3 pty-driven
integration tests. `cargo test --release` on a fresh clone (after
`./setup.sh`) should pass the lot. Tests don't need fixture files —
the pdfium-render crate creates synthetic PDFs in-process; see
`src/pdfhighlights.rs::tests` and `tests/perf_regression.rs` for
patterns.

PRs welcome. Two house rules:

1. **Run the tests** before pushing — this includes the e2e perf
   regression test that catches refactors silently breaking the
   disk-cache or idle-warm wiring.
2. **Match the comment style.** Prefer a one-line "why this is
   non-obvious" over an apology for the code; never write what's
   already in the identifier names.
