# Changelog

All notable changes to `termpdf-rs` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- `Ctrl-L` to manually re-transmit all visible pages — recovery
  hatch for the rare case where Ghostty / tmux drop a cached image
  and the screen goes blank.
- `f=24,o=z` (RGB + zlib) page-transmit format: 40–90% smaller wire
  bytes than the prior PNG path, with comparable encode time at
  zlib level 1. Falls back to PNG via `TERMPDF_TRANSMIT_PNG=1`.
- Long-session stress harness (`tests/stress_long_session.rs`)
  asserting bounded RSS growth and bounded transmit rate over a
  configurable soak window. Wired to a nightly GitHub Actions
  workflow.
- Property tests for `KittyPageRegistry` (LRU + cache + pending-
  delete state machine), 5 invariants, runs at 4× cases in CI.
- Selection bake-into-page-bitmap: selection rects composite into
  the page bitmap before transmit, eliminating the classical-
  placement overlay that caused tmux pane bleed-through.
- Foundation governance docs: LICENSE (MIT), SECURITY.md,
  CONTRIBUTING.md, CODE_OF_CONDUCT.md, THIRD_PARTY_LICENSES.md,
  issue + PR templates, this CHANGELOG.

### Changed
- Release profile: `panic = "abort"`. Drops unwind tables and
  panic=unwind machinery for ~5–10% binary-size reduction. Custom
  panic hook still fires (clears DECSET 2026 before exit).
- Kitty transmit chunk size bumped from 4096 → 131072 base64
  chars per APC packet, reducing per-page protocol overhead on
  large pages.
- Hide terminal cursor on enter, restore on exit.
- Cold-start regression detection: perf job removed from CI
  (cross-machine baseline is unreliable). `tests/perf_baseline.json`
  remains a developer-local tool; nightly stress soak provides the
  long-horizon perf signal in CI.
- Clippy lint thresholds configured crate-wide
  (`Cargo.toml::lints.clippy`) to allow stylistic noise like
  `too_many_arguments` and `doc_lazy_continuation`.

### Fixed
- Pages going blank after long scroll bursts in Ghostty + tmux —
  three-layer mitigation (visible-range pin, Ctrl-L manual refresh,
  smaller wire-byte transmits via f=24,o=z).

### Removed
- Idle auto-refresh — re-transmitted all visible pages every 60 s
  of inactivity as a blanket "in case the terminal lost an image"
  hatch. In practice it caused visible flicker for any reader who
  paused to think for 30 s, and never fired during the active-
  scrolling case where the blank-page bug actually shows up.
  Ctrl-L is the explicit recovery hatch.
- Resurrection clobber in pending-delete state machine: when a
  page was evicted then re-rendered before the queued `a=d` rode
  out, the stale image_id could cancel a different page's
  transmit. Caught by property test, fixed by scrubbing matching
  pending deletes on `mark_transmitted`.
- 60 Hz pending-cold-redraw busy loop during scroll — split
  `ColdRenderDecision` into `DeferBudget` (forces redraw) vs
  `DeferRapid` (does not).
- PDF-open CPU/battery spike — tiered idle policy in
  `main.rs::idle_action`, no per-frame draw + prefetch racing.
- Ghostty crash from missing-image placement spam — rapid-defer
  no longer leaves stale placeholder cells pointing at freed
  image_ids; `clear_page_area` post-placement pass scrubs them.

### Security
- See SECURITY.md for the disclosure channel and threat model.

## [0.1.0] - TBD

Initial public release.

[Unreleased]: https://github.com/amanagr/termpdf-rs/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/amanagr/termpdf-rs/releases/tag/v0.1.0
