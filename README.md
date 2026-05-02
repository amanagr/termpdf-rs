# termpdf-rs

A terminal PDF reader. Renders pages as actual images via the [Kitty
graphics protocol](https://sw.kovidgoenka.net/kitty/graphics-protocol/),
so you get pixel-perfect output inside Kitty, Ghostty, or WezTerm.

Vim-style navigation, color-aware dark mode (red text stays red, blue
charts stay blue), and a `?` overlay listing every key.

## Build

```sh
./setup.sh                # downloads vendor/libpdfium.so (~7.5 MB)
cargo build --release
```

The resulting binary is `target/release/termpdf`. It loads
`vendor/libpdfium.so` at runtime — no static linking, no
LD_LIBRARY_PATH dance. Override the path with `$TERMPDF_PDFIUM` if
you'd rather point at a system pdfium.

## Run

```sh
./target/release/termpdf path/to/file.pdf
./target/release/termpdf -d -p 23 paper.pdf     # dark mode, start at page 23
```

A shell alias is convenient:

```sh
alias pdf='~/termpdf-rs/target/release/termpdf'
```

## Keys

The full keymap lives in the `?` overlay. Highlights:

### Navigation

| Keys                       | Action                                    |
| -------------------------- | ----------------------------------------- |
| `j` / `k`                  | next / prev page (boundary)               |
| `N j` / `N k`              | jump N pages                              |
| Space / `b`                | one screen down / up (less-style)         |
| `Ctrl-d` / `Ctrl-u`        | half-screen down / up                     |
| arrows / `h` / `l`         | pixel-grain scroll (`l`/`h` horizontal)   |
| mouse wheel                | scroll (Shift = horizontal)               |
| `gg` / `G`                 | first / last page                         |
| `N G` / `:<n>` / `:goto N` | jump to page N                            |
| `Ctrl-o` / `Ctrl-i` / Tab  | jumplist back / forward                   |
| `m{a-z}` / `'{a-z}`        | set / jump to mark (persisted per PDF)    |
| `+` / `-` / `0`            | zoom in / out / reset                     |
| `d` / `:set [no]dark`      | toggle dark mode                          |

### Selection (Visual mode — `v`)

| Keys                       | Action                                    |
| -------------------------- | ----------------------------------------- |
| `h` `j` `k` `l`            | move caret by char / line                 |
| `w` / `b` / `e`            | word start / back / end                   |
| `0` / `^` / `$`            | line start / first non-blank / line end   |
| `gg` / `G`                 | first / last char on this page            |
| `f<c>` / `F<c>`            | next / prev `<c>` on this line            |
| `iw` / `is` / `ip`         | inner word / sentence / paragraph         |
| `V` / `Ctrl-v`             | linewise / blockwise selection            |
| `c`                        | cycle highlight color                     |
| `y` / Enter                | save highlight + copy text                |
| `Y`                        | copy text only (no highlight)             |
| `gy`                       | copy as Markdown blockquote w/ citation   |
| click + drag               | highlight with the mouse                  |
| `x` (Normal)               | delete last highlight on current page     |

### Search & TOC

| Keys                       | Action                                    |
| -------------------------- | ----------------------------------------- |
| `/<query>`                 | search the document                       |
| `n` / `N`                  | next / previous match                     |
| `:nohl`                    | clear search results                      |
| `o` / `:toc`               | open outline panel (`/` filters)          |

### Commands & misc

| Keys                       | Action                                    |
| -------------------------- | ----------------------------------------- |
| `:export [path]`           | dump highlights as Markdown notes         |
| `:q` / `q`                 | quit                                      |
| `?`                        | toggle help overlay                       |

### Supported terminals & OS

Linux x86\_64 only at the moment (`setup.sh` downloads a pinned
pdfium build for that target). Best inside Kitty, Ghostty, or
WezTerm. `--protocol sixel` works in xterm/foot; halfblocks is the
last-resort fallback (text is illegible).

Password-protected PDFs are not yet supported — decrypt with
`qpdf --decrypt input.pdf out.pdf` first.

## Why this stack

- **pdfium-render** — Chromium's PDF engine, BSD-3 license, dynamically
  loaded so the Rust crate doesn't have to compile pdfium itself.
- **ratatui + ratatui-image** — picks Kitty graphics on Ghostty
  automatically, falls back to sixel/halfblocks on terminals without it.
- **palette** — luminance-only HSL inversion for dark mode. Naive
  per-channel inversion turns red text into cyan; this is the bug
  almost every PDF dark-mode tool ships.

## Roadmap

v0.1 (this commit):
- Open + render + navigate + dark mode + help overlay + zoom
- Vim-style command mode (`:<n>`, `:q`, `:set [no]dark`)

v0.2:
- Mouse drag-to-highlight, persistent across sessions
- Search via pdfium's `find_text` API
- TOC / outline panel

v0.3:
- EPUB (would need switch to mupdf — pdfium is PDF-only)
- Multi-doc tabs

## Highlights

Stored at `$XDG_DATA_HOME/termpdf-rs/<name>.<hash>.json`. Coordinates
are normalized PDF page-space, so a highlight stays in place across
zoom changes and re-renders.
