# arrow-buffer

- Upstream: https://github.com/apache/arrow-rs, crates.io `arrow-buffer` 60.1.0 (sha256
  7bd46a530bc1a8e763102fc36c1dc0f51f3ffa150c78e7f5658ccb54b1458ade).
- Licence: Apache-2.0 (LICENSE.txt, NOTICE.txt).
- Left out: Cargo.lock and Cargo.toml.orig. The dev-dependencies and benches stay in Cargo.toml,
  unused: the root manifest excludes it from the workspace, so its own tests are not built.

Changes, marked `blaze:`:

1. src/alloc/alignment.rs: `ALIGNMENT` is defined for `wasm64` as for `wasm32` (64 bytes).
   Upstream defines it per `target_arch` with no `wasm64` arm, so on `wasm64-blaze-wasip1` (the
   seed, todo/bootstrap/plan.md) `crate::alloc::ALIGNMENT` resolves to nothing and every
   `MutableBuffer` constructor fails (ix-prof, and blaze-engine through it). Report upstream;
   drop this copy once a release has it.
