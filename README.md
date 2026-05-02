# termpdf-rs

A vim-style PDF reader that lives inside your terminal. Pages render
as actual images via the [Kitty graphics
protocol](https://sw.kovidgoenki.net/kitty/graphics-protocol/), so
text is sharp and figures look right — no halfblocks-ASCII guesswork.

```
┌──────────────────────────────────────┐
│                                      │
│    [a real, pixel-perfect PDF page]  │
│                                      │
│ ─────────────────────────────────────│ ← 3s "you were here" line
│                                      │   after Space-scroll
│                                      │
└──────────────────────────────────────┘
   page 12/300 · zoom 100% · dark · ?
```

## Why you might want this

- **Stays in the terminal.** No window switching, no DE notification
  shuffle. Pair it with `nvim`/`tmux` and the PDF is just another
  pane.
- **Vim keybindings, including text-object selection.** `viw` / `vis`
  / `vip` work on PDF text. Yank as plain text, save a highlight, or
  `gy` to copy a Markdown blockquote with a `— file.pdf, p. 12`
  citation footer.
- **Highlights live in the PDF itself**, not a sidecar JSON. They
  travel with the file, show up in Adobe / Preview / sioyek, and
  survive moves and renames.
- **Color-aware dark mode.** Luminance-only HSL inversion — red text
  stays red, blue charts stay blue. Most PDF dark modes flip every
  channel and turn diagrams into nightmares.
- **Fast.** Stable Kitty image IDs (no terminal-side image leak),
  per-page LRU cache with a configurable byte budget, cell-layer
  selection overlay so dragging never re-encodes the bitmap.

## Try it in 60 seconds

```sh
git clone https://github.com/amanagr/termpdf-rs && cd termpdf-rs
./setup.sh                        # downloads vendor/libpdfium.so (~7.5 MB)
cargo build --release
./target/release/termpdf README.md.pdf   # any PDF works
```

A shell alias is the most useful thing you can do next:

```sh
alias pdf='~/termpdf-rs/target/release/termpdf'
```

Then `pdf paper.pdf` from anywhere. **Press `?` at any time to see
every keybinding.**

## The five keys you'll use most

| Key      | What it does                                            |
| -------- | ------------------------------------------------------- |
| Space    | Scroll one screen down (drops a 3s line at where you were) |
| `j` / `k`| Next / prev page                                        |
| `o`      | Open the table-of-contents panel (`/` to filter)        |
| `v`      | Enter Visual mode → `viw`/`vis`/`vip` → `y` to highlight |
| `?`      | Show every key in a popup                               |

That's enough to read with. Everything else builds on it.

## Daily-use cheat sheet

The full keymap is in the `?` overlay. Here's the high-leverage
subset:

### Navigation

| Keys                       | Action                                    |
| -------------------------- | ----------------------------------------- |
| `j` / `k`                  | next / prev page                          |
| Space / `b`                | one screen down / up (less-style)         |
| `Ctrl-d` / `Ctrl-u`        | half-screen down / up                     |
| `gg` / `G`                 | first / last page                         |
| `:<n>` / `N G`             | jump to page N                            |
| `m{a-z}` / `'{a-z}`        | set / jump to mark (persisted per PDF)    |
| `Ctrl-o` / `Ctrl-i`        | jumplist back / forward                   |
| `+` / `-` / `0`            | zoom in / out / reset                     |
| `d`                        | toggle color-aware dark mode              |

### Highlight & quote (Visual mode — `v`)

| Keys                       | Action                                    |
| -------------------------- | ----------------------------------------- |
| `h j k l` `w b e` `0 ^ $`  | move the caret like in vim                |
| `iw` / `is` / `ip`         | select inner word / sentence / paragraph  |
| `V` / `Ctrl-v`             | linewise / blockwise selection            |
| `c`                        | cycle highlight color                     |
| `y`                        | save highlight + copy plain text          |
| `Y`                        | copy plain text only (no highlight)       |
| `gy`                       | copy as Markdown blockquote w/ citation   |
| click + drag               | highlight with the mouse                  |
| `x` (Normal)               | delete last highlight on current page     |

### Search, TOC, export

| Keys                       | Action                                    |
| -------------------------- | ----------------------------------------- |
| `/<query>` then `n` / `N`  | search · next / prev match                |
| `:nohl`                    | clear search results                      |
| `o`                        | open TOC panel — `/` to filter, Enter jumps |
| `:export [path]`           | dump highlights as a Markdown notes file  |

## Reading PDFs lives nicely with…

```sh
pdf paper.pdf            # in one tmux pane
nvim notes.md            # in another
# In Visual mode: gy copies a Markdown quote with citation.
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
  - **Kitty / Ghostty / WezTerm** — first-class. Pixel-perfect.
  - **xterm / foot / Konsole** — pass `--protocol sixel`.
  - Anything else → halfblocks fallback. Text will be illegible;
    use this only if you just want to confirm it loads.
- **Password-protected PDFs are not yet supported.** Decrypt first:
  ```sh
  qpdf --decrypt locked.pdf unlocked.pdf
  ```

## Troubleshooting

**"Images don't render in tmux."** Run once:
```sh
tmux set -g allow-passthrough on
```
The reader will print a one-time hint. After it shows, an ack-marker
under `$XDG_DATA_HOME/termpdf-rs/.tmux-hint-acked` suppresses it.
Delete that file to re-arm.

**"Nothing renders, just garbage characters."** Your terminal didn't
advertise a graphics protocol. Try `--protocol sixel` or move to
Kitty/Ghostty/WezTerm.

**"Crashed on a big book with annotations."** Fixed in
[a18c0a5](#) — `git pull` and rebuild. The class of bug was a
pdfium quirk on Highlight annotations from other readers.

**"`y` hangs the UI on X11."** Fixed in
[3f86953](#) — was an `xclip -loops` issue.

**"My highlights disappeared."** termpdf-rs writes to the PDF on
clean exit only. If you killed the process with `kill -9`, the
in-memory store wasn't flushed. Plain `q` / `:q` / Ctrl-C all save.

## Configuration

Almost none — termpdf-rs reads three environment variables and a
session file. By design.

| Var                       | Default                            | Meaning                                  |
| ------------------------- | ---------------------------------- | ---------------------------------------- |
| `TERMPDF_PDFIUM`          | `vendor/libpdfium.so` next to bin  | Override path to libpdfium.so            |
| `TERMPDF_PROTOCOL`        | `auto`                             | Force `kitty`/`sixel`/`iterm2`/`halfblocks` |
| `TERMPDF_NO_TMUX_HINT`    | unset                              | Skip the one-time tmux hint              |
| `TERMPDF_CACHE_MB`        | `256`                              | Soft cap on the page-bitmap cache        |

Per-PDF state (current page, dark flag, zoom, marks) lives at
`$XDG_DATA_HOME/termpdf-rs/<name>.<hash>.session.json` (mode 0600).
Same hash scheme means two PDFs with the same name in different
directories don't collide.

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
- No multi-doc tabs.
- No password-protected PDFs (decrypt with `qpdf` first).
- No SyncTeX.
- No PDF form fields or link click-through.
- Two-column papers extract scrambled text on yank/`:export` — known
  bug in the line-clustering algorithm; planned fix splits columns
  before Y-banding.

## Contributing

The code is ~6k LOC of Rust. There's no CI yet; `cargo test --release`
on a fresh clone (after `./setup.sh`) should show 90+ passing tests
including the foreign-Highlight regression test that catches the
class of segfault that bit a real user in early 2026.

PRs welcome — but please run the test suite and consider adding one
for whatever you changed. The pattern in `src/pdfhighlights.rs::tests`
shows how to spin up a synthetic PDF with pdfium-render in-process,
so most coverage gaps don't need fixture files.
