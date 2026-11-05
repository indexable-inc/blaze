# libzstd-rs-sys

- Upstream: https://github.com/trifectatechfoundation/libzstd-rs-sys, git
  b1d9fb6ee245480b03305704913673d6f3ecaeb0 (2026-11-02; crates.io has only 0.0.1-prerelease.2
  of 2026-06-01). The reference zstd translated to Rust by Trifecta Tech Foundation, from
  zstd's dev branch after the 1.5.7 release: it reports 1.5.8 (`ZSTD_VERSION_RELEASE` 8 in
  `lib/zstd.rs`, `"1.5.8"` from `ZSTD_versionString`), a version zstd has not released. Its
  output matching C 1.5.7's is measured, and pinned by blaze-cas's and blaze-format's
  fixtures (vendor/zstd-sys/SOURCE.md), not implied by the version.
- Licence: BSD-3-Clause (LICENSE).
- Left out: everything but `c2rust-lib.rs`, `lib/**/*.rs`, LICENSE and README.md: the C and
  assembly sources, `build.rs` (which assembles `huf_decompress_amd64.S` on x86_64), the
  programs, tests, fuzzers, benches and the workspace. Its `#[cfg(test)]` modules are not built:
  the root manifest excludes it from the workspace.

Built only for `target_family = "wasm"`, as the zstd behind vendor/zstd-sys there (see its
SOURCE.md); native targets use the C library.

Changes, each marked `blaze:`:

1. `Cargo.toml`: no build script, binaries, dev-dependencies or workspace; the library and its
   features as upstream has them.
2. `lib/decompress/huf_decompress.rs`: the three uses of upstream's x86_64 assembly loop are
   `#[cfg(any())]`, since its assembly is not in this copy. Only wasm builds this crate.
3. `lib/common/pool.rs`: on wasm, which can spawn no thread, a job runs to its end inside
   `POOL_add`/`POOL_tryAdd` (`RUN_INLINE`), as upstream C's pool built without
   `ZSTD_MULTITHREAD` runs it. zstdmt's frames depend on how the input is cut into jobs, not on
   where jobs run: blaze-cas's long values (level 6, long-distance matching, 2^27 window, 4
   workers) come out byte for byte as the C library's threads make them (checked 2026-11-02 on
   a store's four long values, 5 to 51 MB).
   Upstream spawned threads, which panics on wasm32-wasip1 ("failed to spawn thread").
   Report upstream. blaze-cas's long fixture (9 MiB, five 2 MiB jobs) runs this path and pins
   its bytes.
4. `lib/decompress/zstd_decompress.rs`: `is_legacy` answers 0 (`LEGACY_SUPPORT` false), so a
   v0.1 to v0.7 frame is a frame of unknown magic, as in the C library blaze builds on native
   targets, which has no legacy decoders: every target reads the same formats.
5. `Cargo.toml`, `c2rust-lib.rs` and the new `libc_unknown.rs`: on wasm32-unknown-unknown (the
   explorer's core, crates/blaze-explore-core, runs in a browser), where the libc crate is
   empty, libc is not a dependency; `extern crate self as libc` makes `libc::` name this
   crate's root, which re-exports `libc_unknown`'s `ptrdiff_t`, `memcpy`, `memcmp`, `malloc`,
   `calloc` and `free`, written in Rust over `core::ptr` and `std::alloc` (a 16-byte header
   holds each block's size for `free`). Only the legacy decoders, which `is_legacy` (change 4)
   never reaches, and a debug assertion and the pool's growth call them. Every other target is
   unchanged.
