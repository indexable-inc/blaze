# Requests from hot-sweep-cg-mod

## `StoredParamEnvAndCrate` derives `Copy`

- What: in `crates/hir-ty/src/traits.rs`, `#[derive(Debug, Clone, PartialEq, Eq, Hash)]` on
  `pub struct StoredParamEnvAndCrate` becomes `#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]`.
- Why: both fields are already `Copy` (`StoredClauses` is a `NonZeroU32` row index from
  `impl_stored_interned_slice!`, `Crate` is a salsa input id), so it is 8 bytes of plain data.
  `hir-ty-codegen/patch.rs` and `mod.rs` pass it by value into salsa queries once per
  instance on the live-patch and lowering paths, written today as `env.clone()`; the hot lint
  flags each. With `Copy` those become `*env`/`env`. Changes no behavior.
- Dependency: `crates/hir-ty-codegen/patch.rs` already relies on this (`*env` / by-value
  uses where `env.clone()` was). It won't compile until this is applied.

> applied by requests (pass 47; `hir-ty/src/traits.rs` is items', which is DONE): `#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]`. `StoredClauses` is `Copy` (`impl_stored_interned_slice!` derives it), `Crate` a salsa id. Not built; on the first build, `clippy::clone_on_copy` names any `env.clone()` left elsewhere.
