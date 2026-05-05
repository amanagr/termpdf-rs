# Security Policy

## Reporting a Vulnerability

If you believe you've found a security vulnerability in `termpdf-rs`, please
report it privately rather than through a public issue.

**Email:** amanagrawal22222@gmail.com

Include in your report:

- A description of the issue and the impact you observed (or expected).
- Steps to reproduce, ideally with a minimal PDF or input file that
  triggers the behavior.
- The version (`termpdf --version`) and platform (terminal, OS, tmux
  presence).
- Whether the issue requires user interaction (opening a malicious PDF)
  or can be triggered passively.

You should expect an acknowledgement within a few days. The fix
timeline depends on severity:

- **Critical** (remote code execution, sandbox escape, credential
  exposure): mitigation released within 7 days, public disclosure
  coordinated with the reporter.
- **High** (denial of service, information leak): mitigation in the
  next release, typically within 30 days.
- **Low** (limited-impact issues, theoretical concerns): tracked as a
  regular issue, fixed when feasible.

## Scope

In scope:

- The `termpdf` binary and the published `termpdf-rs` crate.
- Inputs the binary processes by design: PDF documents, the on-disk
  highlight store (`~/.local/share/termpdf-rs/`), terminal escape
  sequences emitted to stdout/stderr.

Out of scope (please don't report):

- Vulnerabilities in upstream dependencies — report those to the
  respective maintainers (`pdfium-render`, `ratatui`,
  `ratatui-image`, `image`, `crossterm`, etc.). If you find a
  dependency CVE that affects `termpdf-rs`, opening a regular issue
  to track the bump is fine.
- Bugs in the terminal emulator (Ghostty, kitty, tmux). The kitty
  graphics protocol is implemented per-terminal; rendering glitches
  are not security issues.
- Crash-on-malformed-PDF that produces no information leak and no
  RCE. These are reliability bugs, not security bugs — file a regular
  issue.

## Threat Model (informative)

- The PDF parser (pdfium, via `pdfium-render`) is treated as a
  potentially-hostile input boundary. Anything beyond CVE-grade
  pdfium issues should be reported upstream.
- The renderer writes raw kitty graphics escapes to the terminal.
  We do not currently sanitize image bytes against terminal-side
  parsers; if a terminal misbehaves on a crafted bitmap, that's a
  terminal bug. We will, however, work with terminal authors to
  reproduce the issue.
- The on-disk highlight JSON store is written into the user's
  `data_local_dir()`. The format is JSON; we intentionally do not
  unserialize executable content.
