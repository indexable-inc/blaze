# Patches on rustc's `compiler/rustc_transmute`

Base: rust-lang/rust `16d1ffac17ad8b739e32483ac2e65fe7e1804977`, nightly-2026-10-27.

`src/` is upstream's `compiler/rustc_transmute/src` at the base commit with the series below
applied in order (`docs/v2/plan/design/compile-time.md`, change 18). `tools/v2/solver_sync.py`
replaces the base on every toolchain bump and replays the series; it stops at the first patch
that no longer applies, naming it. Each `### NNNN-name` heading is `patches/NNNN-name.patch`, a
diff of `src/` taken with `a/` and `b/` prefixes. To change the series: edit `src/`, regenerate
the patch, and keep this file's headings and the `patches/` directory one for one.

`Cargo.toml` is blaze's, built with `rustc` off, so its dependencies on the compiler's own crates
are left out. `Cargo.toml.orig` is upstream's manifest at the base. The script stops if
upstream's changes, until `Cargo.toml` follows it.

## Series

### 0001-standalone

What: the crate builds with `rustc` off. `Map` and `Set` are `indexmap`'s with
`rustc_hash::FxBuildHasher`, which is what `rustc_data_structures::fx::{FxIndexMap, FxIndexSet}`
are, and `Answer`, `Condition` and `Reason` come from `rustc_type_ir::transmute` (which
`rustc_middle::ty::transmute` re-exports), in the query and in its tests.

Why: upstream names `rustc_data_structures` (a required dependency) and `rustc_middle` (an
optional one) outside the `rustc` feature, so the layout trees, the automata and the query that
are generic over `Def`, `Region` and `Type` don't build without the compiler. Iteration order is
the same: both maps are insertion-ordered.

### 0002-generic-entry

What: what a caller with its own layouts needs is public: `layout::{Tree, Endian, Byte,
Reference}` (`Reference`'s fields too), the `Def`, `Region` and `Type` traits, every `Tree`
builder (`def`, `byte`, `bytes`, `number`, `padding`, `seq`, `alt`, `then`, `or`, `char`,
`nonzero`, ...), `QueryContext`, and `is_transmutable(src, dst, assume, context)`, which runs
the same query `TransmuteTypeEnv::is_transmutable` runs under `rustc`.

Why: hir-ty answers `TransmuteFrom` goals (`SolverDelegate::is_transmutable`) with rustc's
analysis, building each type's `Tree` from its own layouts as rustc's `Tree::from_ty` does
from the compiler's (superset-of-rust). With `rustc` off nothing of the crate was reachable.
Only visibility changes, and the one function; no behavior.
