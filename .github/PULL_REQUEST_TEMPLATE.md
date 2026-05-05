<!--
Thanks for contributing! Before opening, please check:

- [ ] `cargo fmt --all -- --check` is clean
- [ ] `RUSTFLAGS="-D warnings" cargo clippy --all-targets` is clean
- [ ] `cargo test --release` passes locally
- [ ] (perf-sensitive paths) `cargo test --release --test perf_harness`
      shows no regression on your hardware — paste the diff in the
      summary
- [ ] Commit messages follow the conventional-commits style described
      in CONTRIBUTING.md
-->

## Summary

<!-- 1–3 sentences. What does this PR change, and why. -->

## What changed

<!-- Bulleted list of user-visible / behavioral changes. Skip if
this is purely an internal refactor. -->

## Test plan

<!--
Bulleted, copy-paste-ready commands or descriptions of what you ran.
The reviewer should be able to repeat the same checks.

Examples:
- [ ] `cargo test --release`
- [ ] Opened `~/Books/long.pdf` in Ghostty + tmux, scrolled with `j`
      held for 5 s — no stale image warnings in stderr, idle CPU
      back to ~0% within 2 s.
- [ ] Toggled dark mode on a 600-page book — first frame still
      < 100 ms.
-->

## Risk / blast radius

<!-- What could go wrong? What did you check to make sure it
won't? Examples of risk areas: kitty graphics output, tmux
passthrough, dark-mode color drift, on-disk highlight format. -->

## Related issues

<!-- "Fixes #N" / "Refs #N" so the issue closes (or links) on merge. -->
