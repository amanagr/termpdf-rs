# Copy Deck — termpdf-rs marketing site

> Source of truth. Implementer copies verbatim. Voice: terse,
> technical, opinionated, no marketing fluff.

## 1. Headline candidates

1. **"A power-efficient PDF reader that lives inside your terminal — vim keys, kitty-native rendering, near-zero CPU at idle."**
   *Lands with:* the README reader who already knows what they want; safest, broadest framing.
2. **"Read PDFs in your terminal without your fan kicking on."**
   *Lands with:* the laptop-on-battery persona who has lived the Chrome-PDF-fan-spike moment.
3. **"pdfium pages, vim keys, single-digit CPU. In tmux."**
   *Lands with:* the HN scanner who reads three nouns and decides in 2 seconds.
4. **"A PDF reader for people who already live in tmux and Neovim."**
   *Lands with:* the workflow purist who treats the trackpad as friction.
5. **"Pixel-perfect PDF pages in your terminal. Without the browser tax."**
   *Lands with:* the engineer who's annoyed that opening one PDF spins up V8.

## 2. Recommended pair

**Headline:** Read PDFs in your terminal without your fan kicking on.

**Subheadline:** Pixel-perfect pages via the kitty graphics protocol, vim keys, indexed search, highlights stored *in* the PDF. Single-digit CPU during sustained scroll on a 600-page book.

*Rationale:* README hero is excellent in the README, but the homepage
should lead with the discovery moment — *"my fan is at full speed and
I'm just reading a PDF on battery."* Pain first, proof second (kitty,
vim, indexed, in-PDF highlights, the CPU number). README hero stays
available as candidate 1 if the implementer wants to swap.

## 3. One-line install

```sh
git clone https://github.com/amanagr/termpdf-rs && cd termpdf-rs && ./setup.sh && cargo build --release
```

Click-to-copy. `setup.sh` vendors the pinned `libpdfium.so` (~7.5 MB).

## 4. Hero strip — three feature bullets

- **Pixel-perfect pages via the kitty graphics protocol.**
- **Vim keys, vim text-objects, vim marks — on PDF text.**
- **Idle CPU rounds to zero. Held-`j` stays in single digits.**

## 5. Section copy

### Why this exists

Most terminal PDF "readers" either draw halfblocks and pretend that's
reading, or use real graphics protocols but burn CPU as if you were
watching video — a held-`j` on a 600-page book pegs a core, the
laptop discharges while plugged in, the fan kicks on. termpdf-rs is
the inverse: real pixel-perfect images via the kitty graphics
protocol, but the scroll and idle paths are budgeted ruthlessly. Idle
with a PDF open emits effectively no bytes on the pty. A sustained
scroll burst on a 600-page book lands in single-digit CPU.

### Power efficiency

PDF reading is a low-frequency activity — pages turn at human speed,
not video speed — so a reader that pegs CPU on scroll is wasting
battery on nothing. Idle redraws are gated on a dirty flag: when you
do nothing, the binary writes nothing to the pty. Held-key bursts
defer cold pdfium renders entirely; a single settle redraw catches up
when input goes idle. On Designing Data-Intensive Applications, idle
CPU is 3.2% vs. 17.1% in Chrome — 5.3× lower. Sustained held-`j` over
25s: 7.5% combined (termpdf + Ghostty) vs. 66% in Firefox.

### Vim text-objects on PDF

`viw`, `vis`, `vip` work on PDF text the same way they work in
Neovim — inner word, sentence, paragraph, selected from the actual
extracted text. `y` saves the selection as a highlight; `Y` yanks
plain text; `gy` produces a Markdown blockquote with a `— file.pdf,
p. 12` citation footer ready to paste into the notes file open in
the adjacent tmux pane. Visual mode supports `h j k l`, `w b e`,
`0 ^ $`, `V` linewise, `Ctrl-v` blockwise. Marks (`m{a-z}` /
`'{a-z}`) persist per PDF.

### Indexed search

The first search on a new PDF builds a back-index in the background.
Subsequent searches are
[Sioyek-fast](https://ahrm.github.io/jekyll/update/2022/09/11/pdf-viewer-text-search-benchmark.html):
a query matching 5 of 700 pages does 5 pdfium scans, not 700. The
index persists to disk under
`$XDG_CACHE_HOME/termpdf-rs/<file-hash>/`, so re-opening the same PDF
skips the ~3.5 s text-extract cost — search is instant on the second
session forward. `/<query>` to search, `n` / `N` to step matches,
`:nohl` to clear.

### Highlights live in the PDF

Highlights are stored as native `PdfPageAnnotationType::Highlight`
annotations on the PDF itself, not in a sidecar JSON. They travel
with the file, render correctly in Adobe Reader, Preview, Sioyek,
zathura, and survive moves and renames. termpdf-rs adds a small JSON
tag in the annotation's `Contents` field so it can recover the exact
color and any inline note on the next open. Saves are atomic: the new
PDF is written to a sibling tempfile (mode 0600) before pdfium fills
it, then renamed over the original. A crash mid-write leaves the
original untouched.

### Install

Linux x86_64, terminal that speaks the kitty graphics protocol
(Kitty, Ghostty, WezTerm). Clone, `./setup.sh` to vendor the pinned
`libpdfium.so`, `cargo build --release`. No system-wide install — alias
the binary out of `target/release/` and you're done. Sixel and iTerm2
work as fallbacks (`--protocol sixel`); halfblocks loads but text is
illegible. Password-protected PDFs aren't supported yet; decrypt first
with `qpdf --decrypt`.

### FAQ

Quick answers to what people actually ask. Full keymap is in the `?`
overlay; full docs in the README.

## 6. FAQ

**Q: Does this work over SSH?**
A: Yes, if your local terminal speaks the kitty graphics protocol and
the remote shell forwards bytes cleanly. Latency-bound on slow links
because each page is a PNG-sized payload; on a LAN it's fine.

**Q: What about Wayland / X11?**
A: Doesn't care. termpdf-rs renders into your terminal, not into the
display server. If your terminal runs, this runs.

**Q: Does it support encrypted / password-protected PDFs?**
A: Not yet. Decrypt first with `qpdf --decrypt locked.pdf
unlocked.pdf` and open the result. Tracked as a known limitation.

**Q: Can I use this in tmux?**
A: Yes. Run `tmux set -g allow-passthrough on` once so tmux forwards
the kitty graphics escapes. The binary prints a one-time hint if it
detects tmux without passthrough enabled.

**Q: What if my terminal isn't Kitty / Ghostty / WezTerm?**
A: Pass `--protocol sixel` for xterm / foot / Konsole — slower, lower
fidelity, but readable. Anything else falls back to halfblocks, which
is unreadable; treat that as confirmation the binary loads, then
switch terminals.

## 7. OG card text

**Title (≤80 chars):** termpdf-rs — a PDF reader that lives in your terminal

**Subtitle (≤120 chars):** vim keys, kitty-native pixel-perfect pages, indexed search, single-digit CPU on a held-`j`. Linux + Rust.

## 8. Microcopy

- **Install button:** `Copy install command`
- **GitHub button:** `Source on GitHub`
- **Back to top:** `↑ top`
- **404 page main message:** `No page at this offset. The TOC is back at /.`
