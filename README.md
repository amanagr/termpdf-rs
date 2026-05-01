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

| Keys                  | Action                          |
| --------------------- | ------------------------------- |
| `j` `k` Space `b`     | next / prev page                |
| `N j` / `N k`         | jump N pages forward / back     |
| `gg` / `G`            | first / last page               |
| `N G`                 | jump to page N                  |
| `+` `-` `0`           | zoom in / out / reset           |
| `d`                   | toggle dark mode                |
| `v` … `y`             | visual-mode highlight (v0.2)    |
| `/<query>`            | search (v0.2)                   |
| `:<n>`                | jump to page n                  |
| `:q`                  | quit                            |
| `:set dark` / `:set nodark` | dark mode via command     |
| `?`                   | toggle help overlay             |
| `q`                   | quit                            |

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
