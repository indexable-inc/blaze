# Requests from hot-sweep-cg-mod-b

## `StoredParamEnvAndCrate` derives `Copy`

- What: add `Copy` to the derive of `StoredParamEnvAndCrate` in `crates/hir-ty/src/traits.rs`
  (`#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]`).
- Why: its fields are a `StoredClauses` (a `NonZeroU32` row index, already `Copy`) and a
  `Crate` (a salsa id), so it is two `u32`s, but without `Copy` every hot use in
  `hir-ty-codegen` (`made_futures`, `TheirStatics::walk`, `Ready::take`, `instance_body`'s
  callers in `tier.rs`, `imports.rs`, `v2/refs.rs`, `v2/backend.rs`, `units.rs`, `fncache.rs`)
  writes `.clone()`, which `hot_lint.py` flags and which hides that nothing is allocated.
  With `Copy`, the `// clone: two u32s` justifications in `mod.rs` become plain copies.

> applied by requests (pass 47, for hot-sweep-cg-mod's same request): `crates/hir-ty/src/traits.rs` derives `Debug, Clone, Copy, PartialEq, Eq, Hash` on it. Not built.
