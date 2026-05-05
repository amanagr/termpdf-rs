---
name: Bug report
about: A reproducible problem with termpdf-rs
title: "[bug] "
labels: bug
assignees: ''
---

<!--
Before filing, please check:
- Is this a security issue? See SECURITY.md (private email channel).
- Is this a bug in your terminal emulator (not termpdf)? Test with
  `kitty +kitten icat path/to/image.png` or `chafa path/to/image.png`
  in the same terminal first.
- Is this fixed on `main`? Try a build from the latest commit.
-->

## Summary

<!-- One sentence describing what's wrong. -->

## Steps to reproduce

1.
2.
3.

## Expected behavior

<!-- What should have happened. -->

## Actual behavior

<!-- What happened instead. Include exact error messages if any. -->

## Sample PDF

<!--
If the issue is content-dependent, please attach the smallest PDF
that reproduces it. If the PDF is sensitive, describe its
characteristics (page count, page dimensions, embedded fonts, has
links / forms / annotations / scanned pages) and we'll see if we
can build a synthetic equivalent.
-->

## Environment

- termpdf-rs version: <!-- `termpdf --version` -->
- Terminal: <!-- e.g. Ghostty 1.0.x, kitty 0.36.x, foot 1.x -->
- Multiplexer: <!-- tmux 3.x / none / screen / zellij -->
- OS: <!-- output of `uname -a` -->
- Rust toolchain (if building from source): <!-- `rustc --version` -->

## Additional context

<!--
- Does the bug happen every time, or only sometimes?
- Recent changes to your config / terminal / system?
- Any error output to stderr (`termpdf file.pdf 2>err.log; cat err.log`)?
- Paste the relevant lines if any.
-->
