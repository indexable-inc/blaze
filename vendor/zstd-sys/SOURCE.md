# zstd-sys

- Upstream: https://github.com/gyscos/zstd-rs (zstd-safe/zstd-sys), crates.io `zstd-sys`
  2.1.2+zstd.1.5.7 (sha256 481ada9c543b2323cbb7bbdbb5249177fbf0188ed959ed2c15e4b22421c84ed3),
  with the C library of zstd 1.5.7 in `zstd/`.
- Licence: BSD-3-Clause or GPL-2.0 for the C library (zstd/LICENSE, zstd/COPYING), MIT or
  Apache-2.0 for the bindings (LICENSE*).
- Left out: Cargo.lock, Cargo.toml.orig, `wasm-shim/` and `src/wasm_shim.rs` (the allocator shim
  upstream's C needs on wasm, which no longer compiles C there).

This crate is where blaze picks its zstd per target (`one-concept-one-home`): every caller,
blaze's own (blaze-cas, blaze-format) and Arrow's and Parquet's, uses the `zstd` crate, whose
calls land here.

- Native targets (aarch64, x86_64): the C library, built exactly as upstream builds it. It was
  the fastest on aarch64 (2026-11-02, 24 MB of a store's values and 72 MB of its long values,
  8-CPU arm64 ix VM): dictionary level 6 at 245 MB/s compressing and 3350 MB/s decompressing,
  against libzstd-rs-sys's 220 and 2945 and structured-zstd 0.2.6's 176 and 2138 (rustc
  release builds; blaze's release build of the Rust ones was 3 to 8 times slower again).
- `target_family = "wasm"` (the bootstrap seed, which has no C): no C is compiled; the
  declarations in `src/bindings_*.rs` resolve to the symbols libzstd-rs-sys exports
  (vendor/libzstd-rs-sys). That is not zstd 1.5.7: it is a Rust translation of zstd's dev branch
  after 1.5.7, which reports itself as 1.5.8 (`ZSTD_versionNumber` 10508 there, 10507 here).
  Its dictionaries and frames matched the C library's byte for byte on one store's values
  (5131 values and 4 long values, 2026-11-02, handoff /tmp/blaze-lanes/v2-zstd.md). That is a
  measurement, not a guarantee: the fixtures in blaze-cas (`cold.rs`,
  `every_target_writes_the_bytes_c_zstd_1_5_7_wrote`, which includes a 9 MiB frame zstd cuts
  into jobs for its 4 workers) and blaze-format (`tiers/tests.rs`) pin it, and are the check on
  every bump of either library. It was the fastest pure-Rust zstd on wasm32-wasip1 under
  Wasmtime 49: 159 MB/s compressing and 2200 MB/s decompressing at dictionary level 6, against
  structured-zstd's 146 and 1592; ruzstd 0.9.1 has no dictionary encoder.
- Formats: only standard zstd frames, on every target. Nothing in the workspace turns on the
  `legacy` feature (the v0.1 to v0.7 decoders), and libzstd-rs-sys's are switched off to match.
  blaze-cas and blaze-format also refuse any frame whose magic is not `28 B5 2F FD` before
  zstd sees it.

Changes, each marked `blaze:`:

1. `build.rs`: returns before anything is compiled when the target family is wasm.
2. `src/lib.rs`: on wasm, links `libzstd_rs_sys` (`extern crate ... as _`) in place of
   upstream's `wasm_shim` module and its `extern crate alloc`.
3. `Cargo.toml`: libzstd-rs-sys, with `export-symbols`, as a dependency on wasm only; the
   `wasm-shim` headers are out of the package's `include`.
4. `build.rs`: the `wasm-shim` include and its `rerun-if-changed` lines are gone (the wasm
   return before them prints `rerun-if-changed=build.rs`), and `DEBUGLEVEL` no longer asks
   whether the shim is in use.
