# Contributing to termpdf-rs

Thanks for considering a contribution. termpdf-rs aims to be a
pixel-perfect, power-efficient PDF reader inside the terminal —
the bar for changes is correctness, performance, and not
regressing the cool-laptop story.

## Quick start

```sh
git clone https://github.com/amanagr/termpdf-rs.git
cd termpdf-rs
./setup.sh                 # fetches libpdfium.so into vendor/
cargo build --release
./target/release/termpdf path/to/file.pdf
```

`setup.sh` is a one-shot fetch from the upstream
`bblanchon/pdfium-binaries` project; it lands a ~7.5 MB
`libpdfium.so` plus its license under `vendor/`. Both are
gitignored. Re-run only when `setup.sh` itself changes.

## Before you start a non-trivial change

For anything beyond a typo fix or a small bug fix, please open an
issue first to discuss the approach. Bigger refactors and new
features benefit from a 2-message back-and-forth before code is
written — especially in the kitty graphics path, where decisions
have CPU/wire-bytes consequences that aren't obvious from
diff-reading alone.

## What we're looking for

In rough priority order:

1. **Correctness bugs.** Especially around the kitty graphics
   path (blank pages, ghost placements), tmux passthrough, dark
   mode color drift, and selection / highlight persistence.
2. **Power / latency improvements.** termpdf-rs's identity is the
   cool-laptop story. Changes that reduce CPU at idle, reduce
   bytes-on-wire during scroll, or reduce frames-to-first-paint
   are highly welcome — back them with measurements from
   `tests/perf_harness.rs`.
3. **New terminal protocol support.** Sixel and halfblocks already
   work via `ratatui-image`; if you're adding a new protocol,
   please thread the existing fallback chain rather than
   shortcutting it.
4. **Documentation.** README and `docs/ARCHITECTURE.md` are the
   front doors; if a section confused you, a PR clarifying it is
   useful.

## What we're not looking for

- **PRs that broaden the dependency surface.** We deliberately
  cut codec stacks, AV1 encoders, and the chafa fallback because
  they're transitive bloat. New deps need a strong justification.
- **PRs that regress idle CPU.** The idle policy in
  `src/main.rs::idle_action` is load-bearing; raising the
  per-frame redraw rate at idle is a non-starter without a
  matching power-budget argument.
- **Style-only refactors.** If a change is purely "rename / split
  / consolidate" without a correctness or performance angle,
  please discuss in an issue first.

## Code conventions

The codebase is opinionated and the conventions are enforced via
`CLAUDE.md` (used by AI coding agents) and CI. Highlights:

- **No comments that just describe what the code does** — well-named
  identifiers do that. Comments explain *why* (a hidden constraint,
  a benchmark result, a quirk of the kitty protocol).
- **No backwards-compat hacks** for removed code — delete it cleanly.
- **No dead code** — `RUSTFLAGS="-D warnings"` in CI catches
  unused imports and unreachable methods.
- **No new files unless required.** Prefer editing existing files.
  This includes documentation files; don't add a new `*.md`
  unless the user-facing surface area justifies it.
- **No emojis in code or comments.**

`cargo fmt` and `cargo clippy --all-targets` must pass.
Stylistic clippy lints are allowlisted in `Cargo.toml` (search
for `[lints.clippy]`); add to that list rather than scattering
`#[allow(...)]` if you hit a recurring false positive.

## Testing

Three tiers, all run in CI:

```sh
cargo test --release --bin termpdf            # unit tests (262)
cargo test --release --test render_kitty      # kitty-graphics smoke
cargo test --release --test render_pty        # pty-driven smoke
PROPTEST_CASES=256 cargo test --release \
    --bin termpdf registry_proptests          # 4× property tests
```

The PTY-driven test (`render_pty`) spawns the built `termpdf`
binary inside a `portable-pty` and asserts on the exact bytes the
renderer emits. If your change affects what's written to stdout
(escape sequences, kitty graphics, status bar text), this test
will catch it — please update the expected sequence rather than
silencing it.

For perf-sensitive changes, run `cargo test --release --test
perf_harness` locally and include before/after numbers in the PR
description. The baseline is per-machine; CI does **not** gate on
perf because runner hardware is not your hardware.

## Commit messages

We use a lightweight conventional-commits style:

```
type(scope): short summary in imperative mood

Optional longer body explaining *why* the change is needed
and what alternatives were considered. Reference issues with
"Fixes #N" / "Refs #N".
```

`type` is one of: `feat`, `fix`, `perf`, `refactor`, `test`,
`docs`, `build`, `ci`, `cleanup`. `scope` is optional and
typically a module name or a feature area (`kitty`, `pty`,
`selection`).

## Reporting bugs

Use the bug report template at
`.github/ISSUE_TEMPLATE/bug_report.md`. The most useful
reports include:

- Terminal name + version (`Ghostty 1.x`, `kitty 0.x`, `tmux 3.x`).
- Whether you're inside tmux/screen/zellij.
- A small PDF that reproduces, if the issue is content-dependent.
- The output of `termpdf --version` and `uname -a`.

Security-relevant bugs go to `SECURITY.md` instead — please don't
file them as public issues.

## License

By contributing you agree that your contribution will be licensed
under the MIT License (see `LICENSE`). We use the standard
inbound=outbound model — no separate CLA.
