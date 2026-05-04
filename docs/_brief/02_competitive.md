# Competitive Analysis — termpdf-rs

> Source for the homepage "Why this exists" section, the
> README-vs-browser table, and the OG card subtitle. Stay specific;
> stay honest. HN comments will catch any embellishment.

## 1. Comp matrix

| Competitor                                                              | kitty-graphics | vim keys | search index   | highlights-in-PDF | dark mode    | in-terminal  | low-CPU       | last meaningful update |
| ----------------------------------------------------------------------- | -------------- | -------- | -------------- | ----------------- | ------------ | ------------ | ------------- | ---------------------- |
| **termpdf-rs**                                                          | yes (native)   | yes      | yes (persisted)| yes (PDF annot)   | luminance HSL| yes          | yes (~3% idle)| 2026-05               |
| [Sioyek](https://github.com/ahrm/sioyek) (9.5k★)                        | no (Qt6 GUI)   | partial  | no (re-search) | yes               | yes          | no           | n/a (GUI)     | v2.0.0 Dec 2022        |
| [Zathura](https://github.com/pwmt/zathura) (3.1k★)                      | no (GTK GUI)   | yes      | no             | no (sidecar only) | basic recolor| no           | n/a (GUI)     | 0.5.14 Oct 2025        |
| [tdf](https://github.com/itsjunetime/tdf) (1.7k★)                       | yes            | partial  | yes (live)     | no                | no           | yes          | unknown       | v0.5.0 Dec 2025        |
| [MeowPDF](https://github.com/monoamine11231/MeowPDF) (74★)              | yes            | yes      | no             | no                | invert only  | yes          | unknown       | v1.2.2 Jan 2026        |
| [termpdf.py](https://github.com/dsanson/termpdf.py) (603★)              | yes            | yes      | no             | partial (sidecar) | invert only  | yes          | unknown       | alpha, low activity    |
| [fbpdf](https://github.com/aligrudi/fbpdf) (221★)                       | no (framebuffer)| yes     | no             | no                | no           | console only | low           | mupdf-era, sporadic    |
| mupdf-cli / mutool                                                      | no             | partial  | no             | no                | no           | no (X11 win) | low           | active (Artifex)       |
| Evince / Okular                                                         | no (GUI)       | no       | no             | yes (Okular)      | yes          | no           | n/a           | active (GNOME/KDE)     |
| qutebrowser PDF mode (11.5k★)                                           | no             | yes (chrome) | via pdf.js | no                | dim only     | no           | high (Qt+pdfjs)| v3.7.0 Apr 2026       |
| Chrome / Firefox built-in                                               | no             | no       | yes            | no (session-local)| dim only     | no           | high (~17% idle)| ongoing              |
| Preview.app / Adobe Reader DC / Foxit                                   | no             | no       | yes            | yes               | yes          | no           | n/a (macOS/Win)| ongoing               |

**Best alternative we displace: Zathura.** It's been the canonical
"vim-keys PDF reader on Linux" for a decade, but it's still a GTK
window that breaks the tmux-only flow. termpdf-rs is what someone
reaches for when they wanted Zathura *inside* their multiplexer with
a pixel-perfect render and a CPU graph that survives a flight.
Sioyek is the "best-in-class GUI" comparator on research-paper
features, but it's a Qt window and last shipped in 2022.

## 2. Differentiation — where we uniquely win

1. **Vim text-objects on extracted PDF text (`viw`/`vis`/`vip` →
   `gy`).** The moat: this needs a real layout engine that knows
   word/sentence/paragraph boundaries on the rendered PDF, plus a
   selection model that maps caret movement back to glyph runs. No
   other terminal PDF reader has both. tdf has search but no
   selection; MeowPDF has GUI-style click-drag but no `iw`.

2. **Power efficiency as a first-class product axis.** ~3.2% idle CPU
   vs. ~17% in Chrome on a 600-page book; ~7.5% during sustained
   held-`j` vs. ~66% in Firefox. The moat: this is a dirty-flag
   architecture decision baked into the run loop (idle redraws gated,
   held-key burst defer, tiered cache) — competitors built around a
   60Hz GUI compositor or a video-style render loop can't bolt this
   on without a rewrite.

3. **Highlights stored as native PDF annotations, not sidecars.** The
   moat: requires writing valid PDF `/Highlight` annotation objects
   atomically over the original file (mode 0600 tempfile + rename).
   Zathura punts to a sidecar; tdf has none; only the GUI heavyweights
   (Okular, Preview, Adobe) ship this — and none of them are in a
   terminal.

4. **Color-aware HSL dark mode.** The moat: most "PDF dark mode"
   ships a channel-flip and the red/cyan inversion bug ships with it.
   We invert luminance only via `palette` so red text stays red, blue
   charts stay blue. Browsers don't even attempt this for PDFs.

5. **Persisted full-text search index.** The moat: first search builds
   a back-index in the background, persists it to
   `$XDG_CACHE_HOME/termpdf-rs/<file-hash>/`, subsequent opens are
   instant. Sioyek search is fast-but-not-indexed; Zathura is per-page
   linear; Chrome/Firefox start over each session.

## 3. Honest weaknesses

- **Image-heavy magazines and full-page color photo PDFs** feel
  snappier in a GUI reader. Pdfium decode + PNG encode + kitty
  upload is multi-MB per page; native GUIs can hand RGBA straight
  to the GPU compositor with no pty round-trip.
- **No multi-doc tabs.** Sioyek, Zathura, and every browser PDF mode
  let you flip between papers in one window. We're one PDF per
  process by design — fine in tmux, awkward without it.
- **Linux x86_64 only.** macOS and Windows users (a meaningful
  chunk of the academic persona) need to wait for the libpdfium
  port or use Sioyek today.

## 4. Adjacent-tool framing — when to use the other thing

- **Sioyek:** if you're an academic moving across 5 papers at once
  with portals/overview windows on a multi-monitor desk and don't
  care that it's a GUI window — Sioyek's research-paper feature set
  is unmatched and termpdf-rs doesn't try to compete on portals.
- **Zathura:** if you want a battle-tested vim-keys PDF reader,
  you're not in tmux, and you don't need indexed search or
  in-PDF highlights — Zathura has shipped consistently since 2009
  and the seccomp sandbox is nice.
- **Browser PDF (Chrome/Firefox):** if you're opening one PDF for
  60 seconds to check a page number and you already have the browser
  open — the startup cost is zero and CPU doesn't matter for one
  minute.

## 5. Discovery comparison — where their users live, where we can reach them

- **Sioyek** — repeat HN front-pages (2021, 2022, 2024) and the
  academic Twitter/Mastodon orbit; the Codeforces blog post drives
  steady CS-grad-student traffic. *Reachable* on lobste.rs and a
  future HN post framed "Sioyek-style search, but in your terminal."
- **Zathura** — ArchWiki, r/unixporn dotfile screenshots, r/archlinux,
  distro-packaged everywhere. *Reachable* via r/archlinux, r/unixporn
  (a tmux pane with a rendered PDF + CPU graph), and "zathura
  alternative" search-intent.
- **tdf / MeowPDF / fancy-cat** — adjacent Rust/TUI builders found via
  GitHub trending and r/rust. tdf (1.7k★) is the closest by category;
  we beat it on selection, highlights, dark mode, and search
  persistence. *Reachable* via `awesome-tuis`, `awesome-ratatui`, and
  a "what tdf doesn't have" angle on r/rust.
- **Browser PDF users** — the largest pool, hardest to convert; they
  don't search "terminal pdf reader." Reachable only via the HN
  benchmark headline ("17% → 3% CPU on the same PDF") making them
  notice the cost they were paying.

Sources: [Sioyek](https://github.com/ahrm/sioyek),
[Zathura](https://github.com/pwmt/zathura),
[tdf](https://github.com/itsjunetime/tdf),
[MeowPDF](https://github.com/monoamine11231/MeowPDF),
[termpdf.py](https://github.com/dsanson/termpdf.py),
[fbpdf](https://github.com/aligrudi/fbpdf),
[qutebrowser](https://github.com/qutebrowser/qutebrowser),
[Sioyek HN 2024](https://news.ycombinator.com/item?id=39770131),
[Zathura 0.5.14 release notes](https://pwmt.org/projects/zathura/).
