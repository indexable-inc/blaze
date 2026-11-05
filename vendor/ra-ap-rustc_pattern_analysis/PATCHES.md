# Patches on crates.io `ra-ap-rustc_pattern_analysis`

Base: crates.io `ra-ap-rustc_pattern_analysis` 0.179.0, sha256 `978b57ee433aec2cd5775546ade5cc04d84c060240d1e3486cce79736e46841c`.
Left out: `Cargo.lock`, `.cargo_vcs_info.json`.

This directory is the published `.crate` at the base (rust-analyzer's automatic publish of
rust-lang/rust's `compiler/rustc_pattern_analysis`), without the entries left out, with the
series below applied in order (docs/v2/plan/observe/no-tracing.md, section 6.2).
`tools/vendor/sync.py` re-fetches the base (checked against the crates.io index and
`Cargo.lock`), replays the series and stops at the first patch that no longer applies, naming it
and each failing hunk; run it on every bump. Each `### NNNN-name` heading is
`patches/NNNN-name.patch`, a diff of the whole crate taken with `a/` and `b/` prefixes. To change
the series: edit the tree, regenerate the patch as the diff between its stages, and keep this
file's headings and `patches/` one for one.

`Cargo.toml` is the published (normalized) manifest with the series applied; `Cargo.toml.orig`
is the one the `.crate` ships, never patched. blaze's root `Cargo.toml` patches
`ra-ap-rustc_pattern_analysis` to this directory and excludes it from the workspace, so its
dev-dependencies stay out of blaze's lock.

Gate: `blaze test` run in this directory (its own workspace) builds and passes the crate's own
tests (`tests/complexity.rs`, `tests/exhaustiveness.rs`, `tests/intersection.rs`).

## Series

### 0001-no-tracing

What: every `tracing` use is deleted: the `debug!` statements and the three debug-level
`#[instrument]`s (`collect_nonexhaustive_missing_variants`, `compute_exhaustiveness_and_usefulness`,
`compute_match_usefulness`; their `ret` and argument fields are dropped), and in the tests
`init_tracing` (a `tracing-tree` layer). The manifest drops `tracing` and the dev-dependencies
`tracing-subscriber` and `tracing-tree`.

Why: blaze drops `tracing` (docs/v2/plan/observe/no-tracing.md); pattern analysis is
third-party code whose logs blaze does not record, so the sites go rather than become `trace`
sites (those are for hir-ty and the solver, section 5.1).

### 0002-tests-name-the-published-crate

What: each integration test root gets
`extern crate ra_ap_rustc_pattern_analysis as rustc_pattern_analysis;`.

Why: the tests name the crate `rustc_pattern_analysis`, as rust-lang/rust builds it, but the
crate is published as `ra_ap_rustc_pattern_analysis`, so as shipped they do not compile; the
alias makes the crate's own tests the gate without touching their bodies.
