# Audience & Personas — termpdf-rs

> Source for homepage copy, OG card title, README hero. Be specific
> when in doubt. Generic "developer" framing is a fail.

## Primary persona — "Linux terminal-native, on battery, reading a 600-page tech book in tmux"

**Sketch:** 27-38, backend / infra / systems engineer, runs Linux as a
daily driver (Arch, Fedora, NixOS, Debian unstable). Lives in tmux.
Editor is Neovim. Terminal is Kitty, Ghostty, or WezTerm — picked
specifically because they speak a real graphics protocol. Strong
preference for keyboard-only flows; treats reaching for the trackpad
as friction.

**The job they're doing:** reading *Designing Data-Intensive
Applications*, the Raft paper, an RFC, the Linux kernel networking
docs PDF, or a 200-page vendor whitepaper they have to skim before a
1pm meeting. Notes go into a Markdown file open in nvim in an adjacent
tmux pane. They want to highlight a paragraph, get a Markdown
blockquote with citation, paste into notes, keep reading. No
context-switch to a GUI app, no Chrome window stealing focus.

**The specific pain termpdf-rs uniquely solves:** they tried `zathura`
(GUI, breaks the tmux-only flow), `pdftotext | less` (figures
unreadable, layout dead), Chrome (fan kicks on, battery falls 8% in
20 min of reading on a held-`j`), Sioyek (great, but it's a window —
not in tmux). The discovery moment is *"my fan is at full speed and
I'm just reading a PDF on battery."* They want pixel-perfect pages
**inside** tmux, vim keys, and a CPU graph that doesn't look like
they're transcoding video.

## Secondary persona — "Academic / grad student writing a lit review"

PhD student or postdoc in CS / EE / stats who already lives in
Neovim + LaTeX + Zotero. Reads 10-30 papers a week, wants to yank
quotes into Obsidian or a `.bib`-adjacent notes file, wants
highlights to *survive in the PDF* so re-opening in Preview or
Sioyek next year still shows them. Less hardcore about CPU; cares
deeply about `gy` producing a Markdown blockquote with `— file.pdf,
p. 12` so they don't manually type citations. Two-column-paper bug is
a known limitation we should be upfront about for this group.

## Anti-persona — "Casual PDF user on macOS / Windows"

Opens 1-2 PDFs a week, double-clicks them, uses Preview / Edge / Acrobat.
Doesn't know what tmux is, doesn't have a graphics-capable terminal
installed, would type "how do I scroll" into our GitHub issues. Also
not for: people who need form-filling, e-signatures, password-protected
PDFs, EPUBs, or two-page book-spread layout. Saying "no" up front
keeps the issue tracker focused.

## Their language (verbatim search queries)

- "terminal pdf reader vim keys"
- "kitty graphics pdf"
- "pdf reader low cpu battery"
- "zathura alternative tmux"
- "sioyek but in terminal"
- "neovim pdf workflow"
- "my fan kicks on when reading pdfs"
- "pdf reader that doesn't burn battery"
- "highlight pdf from terminal"
- "vimium for pdf links"

## Discovery channels (top 3)

1. **Hacker News** — Show HN with the headline benchmark ("17% → 3%
   CPU reading a PDF"). Audience overlaps near-perfectly: HN crowd
   runs Linux, lives in terminals, has strong opinions on browser
   bloat. The "battery" + "Rust" + "vim" + "Kitty" stack is catnip.
2. **r/neovim, r/commandline, r/kitty, lobste.rs `rust`/`unix`
   tags** — same persona, slower-burn discovery. r/neovim
   especially — the `gy → Markdown blockquote → paste into nvim`
   loop is the exact workflow they're already optimising.
3. **GitHub topic search + awesome-lists** — `pdf-viewer`,
   `terminal`, `kitty-graphics-protocol`, `tui`, `ratatui`. Add to
   `awesome-rust`, `awesome-tuis`, `awesome-ratatui`. Long tail but
   high-intent: these users arrive already convinced they want a TUI.
