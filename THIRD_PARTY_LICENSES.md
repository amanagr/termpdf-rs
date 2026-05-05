# Third-Party Licenses

`termpdf-rs` is distributed under the MIT License (see [LICENSE](LICENSE)).
The compiled binary statically links Rust dependencies fetched by Cargo
and dynamically loads `libpdfium.so` at runtime. This document summarizes
the licenses of those dependencies.

## License summary (319 transitive crates)

| Count | License                                              |
|------:|------------------------------------------------------|
|   168 | MIT OR Apache-2.0 (dual)                             |
|    69 | MIT                                                  |
|    20 | MIT/Apache-2.0 (dual, slash form)                    |
|    17 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT  |
|    10 | Apache-2.0 OR MIT                                    |
|     4 | Unlicense OR MIT                                     |
|     4 | Apache-2.0/MIT                                       |
|     4 | Zlib OR Apache-2.0 OR MIT                            |
|     2 | Zlib                                                 |
|     2 | BSD-3-Clause OR Apache-2.0                           |
|     2 | MIT OR Apache-2.0 OR LGPL-2.1-or-later               |
|     2 | BSD-2-Clause OR Apache-2.0 OR MIT                    |
|     2 | MIT OR Apache-2.0 OR Zlib                            |
|     1 | 0BSD OR MIT OR Apache-2.0                            |
|     1 | Apache-2.0                                           |
|     1 | MIT OR Apache-2.0 OR CC0-1.0                         |
|     1 | (MIT OR Apache-2.0) AND Unicode-DFS-2016             |
|     1 | Apache-2.0 / MIT                                     |
|     1 | ISC                                                  |
|     1 | MIT OR Zlib OR Apache-2.0                            |
|     1 | MPL-2.0                                              |
|     1 | Apache-2.0 OR BSL-1.0                                |
|     1 | BSD-2-Clause OR Apache-2.0                           |
|     1 | WTFPL                                                |
|     1 | (MIT OR Apache-2.0) AND Unicode-3.0                  |
|     1 | MIT AND Unicode-DFS-2016                             |

All Rust dependencies are statically linked into the binary under
permissive terms compatible with MIT distribution.

## Notable third-party dependencies

### libpdfium (Apache-2.0 / BSD-3-Clause)

`libpdfium.so` is a Google-maintained PDF rendering library. It is
**not** included in the `termpdf-rs` source tree and is **not**
statically linked. It is fetched at build time by `setup.sh` from the
[`bblanchon/pdfium-binaries`](https://github.com/bblanchon/pdfium-binaries)
prebuild project, and loaded via `dlopen` at runtime by
`pdfium-render`.

Released binaries that bundle `libpdfium.so` redistribute under the
combined Apache-2.0 / BSD-3-Clause terms documented in the upstream
project. The `vendor/LICENSE.txt` file shipped alongside the
prebuilt library carries the full text.

### MPL-2.0 dependency

One transitive dependency — `option-ext` — is licensed under
**MPL-2.0**. We do not modify its source, so the only obligation is
attribution; the MPL's file-level copyleft does not extend to the
binary as a whole.

### Unicode-licensed dependencies

`finl_unicode` and `icu_*` crates carry **Unicode-DFS-2016** /
**Unicode-3.0** terms in addition to MIT/Apache. These permit
redistribution under the MIT terms used by this project.

### Apache WITH LLVM-exception

17 transitive crates from the Rust ecosystem (e.g. `wasm-bindgen`)
carry the LLVM exception clause. This relaxes the patent-grant
language; it is fully permissive for our use.

## Generating the full list

To regenerate the list (e.g. before a release):

```sh
cargo install cargo-about
cargo about generate -o THIRD_PARTY_LICENSES_FULL.html
```

The full HTML output includes complete license text for every
transitive crate. We don't check it into the repo because it churns
on every dependency bump; release tarballs include a snapshot.

## Reporting a license issue

If you spot a dependency whose license does not appear above, or
believe our license summary is incorrect, file an issue at:
https://github.com/amanagr/termpdf-rs/issues

For dependency-license compliance issues, an issue on this repo is
sufficient — we do not require private disclosure for license matters.
