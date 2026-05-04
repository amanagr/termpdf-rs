# Launch & Deployment — termpdf-rs

> Site: **https://amanagr.github.io/termpdf-rs/** (`<user>.github.io/<repo>/`. Confirmed.)

## 1. GitHub Pages — click-by-click for Aman

1. Push `/docs/` to `main` first; Pages can't enable on a missing folder.
2. Open https://github.com/amanagr/termpdf-rs/settings/pages
3. **Build and deployment → Source**: select **Deploy from a branch**.
4. **Branch**: `main`, folder `/docs`. Click **Save**.
5. Wait ~30-90 s, refresh. Green banner: *"Your site is live at https://amanagr.github.io/termpdf-rs/"*.
6. Verify: hard-reload (`Ctrl-Shift-R`), CSS loads, OG preview at opengraph.xyz/url/<your-url>.
7. If you see a raw README or 404, `.nojekyll` is missing — check with `ls -la docs/`.
8. Confirm **Enforce HTTPS** is on.

No custom domain at launch — keep the github.io URL so the stars link is one click from the URL bar.

## 2. Files the implementing agent must create in `/docs/`

| Path | Purpose |
| --- | --- |
| `index.html` | One-page site. Hero (README headline), 3-column "what you get" grid, CPU% table, install snippet, keybinding preview, GitHub link. No JS framework. |
| `style.css` | Separate file (cacheable; HTML stays diffable). Dark default. System font stack — no webfont request. |
| `.nojekyll` | Empty file. Tells Pages "skip Jekyll." Without it, `_brief/` and any liquid-looking syntax break. **Mandatory.** |
| `404.html` | Same shell as `index.html`; body links to `/` and the repo. Pages serves it on 404. |
| `og.svg` + `og.png` | Both. Slack/iMessage crawlers are inconsistent on SVG OG cards. Implementer renders SVG → 1200×630 PNG once with `rsvg-convert -w 1200 -h 630 og.svg -o og.png`; `<meta property="og:image">` points at the PNG. |
| `robots.txt` | `User-agent: *` / `Allow: /` / `Sitemap: https://amanagr.github.io/termpdf-rs/sitemap.xml`. |
| `sitemap.xml` | One `<url>` for the root, `<lastmod>` = commit date. |
| `favicon.svg` | A `t.` glyph or kitty-graphics motif. SVG so it scales. |

Skip `CNAME`. Embed JSON-LD SoftwareApplication in `index.html` so Google's snippet shows version + license.

## 3. README updates (`/home/aman/termpdf-rs/README.md`)

Add **two** website references:

- **After line 16** (closing `</div>` of the hero), before the `---` on line 18:
  `<div align="center"><a href="https://amanagr.github.io/termpdf-rs/">Website</a> · <a href="https://github.com/amanagr/termpdf-rs">GitHub</a></div>`
- **After line 84** (the `./target/release/termpdf paper.pdf` line, before the alias paragraph):
  `> Full docs, screenshots, and the power-efficiency writeup: https://amanagr.github.io/termpdf-rs/`

Top of file for browsers, install section for skimmers. Don't sprinkle further.

## 4. GitHub repo metadata (Settings → "About" gear icon)

**Topics (7):** `rust`, `pdf`, `pdf-viewer`, `terminal`, `tui`, `kitty-graphics-protocol`, `vim`

(Skip `ratatui` — too niche for search. Skip `cli` — too broad.)

**Description (≤350 chars):**
> Power-efficient terminal PDF reader in Rust. Pixel-perfect pages via the Kitty graphics protocol, vim keybindings, indexed full-text search, color-aware dark mode, and highlights stored inside the PDF. Single-digit CPU during scroll on a 600-page book — 9× lower than a browser.

**Website field:** `https://amanagr.github.io/termpdf-rs/`

## 5. "Show HN" launch post (~260 words)

> **Show HN: termpdf-rs — a terminal PDF reader that doesn't melt your laptop**
>
> I read a lot of PDFs — papers, RFCs, 600-page tech books — and the existing options annoyed me. Browsers ship the entire web platform to render a static document and burn 60-70% of a CPU core when I hold `j` to scroll. Zathura and Sioyek are great but they're GUI windows, and my whole flow is tmux + Neovim. Halfblock-ASCII "terminal PDF readers" exist but are unreadable on anything with a figure.
>
> termpdf-rs is the inverse: real pixel-perfect pages via the Kitty graphics protocol (also spoken by Ghostty and WezTerm), but the scroll path is budgeted ruthlessly. Held-`j` on a 600-page book lands at ~7.5% CPU combined (binary + terminal); Firefox on the same file is 66%. Idle writes effectively zero bytes to the pty — gated behind a dirty flag, so the fan doesn't even know.
>
> Vim keybindings throughout — `viw`/`vis`/`vip` text-objects work on PDF text, `gy` yanks a Markdown blockquote with a `— file.pdf, p. 12` citation ready to paste into nvim. Highlights are native PDF annotations — they travel with the file and render in Adobe / Preview. Color-aware dark mode that doesn't turn red text into cyan. Vimium-style link-follow. Indexed search.
>
> Stack: pdfium-render (Chromium's PDF engine, dynamically loaded), ratatui + ratatui-image, palette. Linux x86_64 only at launch — would love help porting.
>
> Repo, docs, reproducible benchmarks: https://amanagr.github.io/termpdf-rs/

## 6. Distribution plan (5 channels, ordered)

The persona (Linux + tmux + nvim + Kitty/Ghostty engineer) lives in a handful of places. Don't dilute.

1. **Hacker News — Tue/Wed, 8:30 am Pacific.** Title: *"Show HN: termpdf-rs — a terminal PDF reader that doesn't melt your laptop"*. Body = §5. Highest leverage; everything else feeds off this thread.
2. **/r/rust — same day, +4 h.** Title: *"termpdf-rs: a power-efficient PDF reader in Rust with Kitty graphics + vim keys"*. Lead with the pdfium-render + ratatui-image stack (this sub reads stack before product).
3. **/r/commandline — same day, +6 h.** Title: *"I built a terminal PDF reader that's 9× lower CPU than a browser during scroll"*. Lead with the comparison-table screenshot.
4. **Lobste.rs — next morning.** Tags: `rust`, `release`. One paragraph; link the site, not the HN thread.
5. **Mastodon (#rustlang #linux #vim) — within the launch hour.** Boosts are where the Ghostty / WezTerm / Kitty maintainer crowd picks it up.

**Skip:** /r/vim (off-topic, downvoted), ratatui Discord (small; better as a "thanks for the lib" follow-up *after* launch), Product Hunt (wrong audience).

## 7. Post-launch additions (~72 h in)

1. **A 30-second asciinema/terminalizer cast** at the top of the page, replacing the ASCII-art hero: open PDF → scroll → `f` link-follow → `v viw y` highlight → `gy` paste into nvim. The most-requested HN comment will be "is there a video?".
2. **A comparison page (`/docs/vs.html`)** — termpdf-rs vs. zathura vs. Sioyek vs. Chrome on a fixed grid: idle CPU, scroll CPU, RSS, vim keys, runs in tmux, highlights in PDF. What readers who almost-installed-but-bounced will Google.
3. **GitHub Sponsors link in About** — once ~20 inbound issues land. Don't lead with it; add once the project has proved itself.
