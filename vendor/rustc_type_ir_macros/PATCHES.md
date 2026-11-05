# Patches on rustc's `compiler/rustc_type_ir_macros`

Base: rust-lang/rust `16d1ffac17ad8b739e32483ac2e65fe7e1804977`, nightly-2026-10-27.

`src/` is upstream's `compiler/rustc_type_ir_macros/src` at the base commit with the series
below applied in order (`docs/v2/plan/design/compile-time.md`, change 18). It replaces the
crates.io `ra-ap-rustc_type_ir_macros`, so the derives match the `rustc_type_ir` beside them.
`tools/v2/solver_sync.py` replaces the base on every toolchain bump and replays the series; it
stops at the first patch that no longer applies, naming it. Each `### NNNN-name` heading is
`patches/NNNN-name.patch`, a diff of `src/` taken with `a/` and `b/` prefixes.

`Cargo.toml` has upstream's dependencies unchanged. `Cargo.toml.orig` is upstream's manifest at
the base, and the script stops if upstream's changes, until `Cargo.toml` follows it.

## Series

### 0001-derived-generic-type-visitable-allows-unsafe-code

What: `#[derive(GenericTypeVisitable)]` puts `#[allow(unsafe_code)]` on the `const _` item that
holds its `unsafe impl`.

Why: `GenericTypeVisitable` became an `unsafe trait` upstream. The derive is what upholds its
contract, since it visits every field of every variant. `rustc_type_ir` and the crates deriving
it deny `unsafe_code` (`docs/v2/plan/design/unsafe.md`). This changes nothing for a program
that doesn't use these derives.
