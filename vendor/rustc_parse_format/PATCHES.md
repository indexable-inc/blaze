# Patches on rustc's `compiler/rustc_parse_format`

Base: rust-lang/rust `16d1ffac17ad8b739e32483ac2e65fe7e1804977`, nightly-2026-10-27.

`src/` is upstream's `compiler/rustc_parse_format/src` at the base commit with the series below
applied in order, so format strings (`format_args!`, `asm!`, `panic!`) parse as the pinned nightly
parses them. `tools/v2/solver_sync.py` replaces the base on every toolchain bump and replays the
series; it stops at the first patch that no longer applies, naming it. Each `### NNNN-name`
heading is `patches/NNNN-name.patch`, a diff of `src/` taken with `a/` and `b/` prefixes. To
change the series: edit `src/`, regenerate the patch, and keep this file's headings and the
`patches/` directory one for one.

`Cargo.toml` is blaze's: upstream's dependencies, with `rustc_lexer` the pinned nightly's
(`crates/rustc_lexer`) and the dev-dependency `rustc_index` rust-analyzer's crates.io release.
The crate has no `nightly` feature and denies `unstable_features`. `Cargo.toml.orig` is
upstream's manifest at the base. The script stops if upstream's changes, until `Cargo.toml`
follows it.

## Series

None: `src/` is upstream's unchanged.
