---
status: plan
summary: index of the speed-of-light docs for db-ty's components (hir-ty, hir, hir-ty-borrowck, hir-ty-wfcheck), one bullet per component, each naming the doc that holds its bound, arithmetic and design.
date: 2026-10-29
---
# Speed of light: db-ty's components

db-ty owns `crates/hir-ty/` (but lane items' files), `crates/hir/`, `crates/hir-ty-borrowck/`
and `crates/hir-ty-wfcheck/`. Each component's bound, the arithmetic behind it, what the code
costs today and the design that reaches 90% of it are in the doc named below. The machine
figures are the ones at the top of [speed-of-light.md](speed-of-light.md). Nothing is measured:
costs are counted from the code, and `proof`'s budgets measure them once the tree builds.

One bullet per component; add yours, don't rewrite another's.

- Trait solver glue (fulfillment, normalization, proof-tree inspection, `next_solver/`):
  [speed-of-light-solver-glue.md](speed-of-light-solver-glue.md).
- Trait solver cache lookup (`next_solver/interner/tls_cache.rs`):
  [speed-of-light-solver-cache.md](speed-of-light-solver-cache.md).
- Type relations (`next_solver/infer/relate/`):
  [speed-of-light-infer-relate.md](speed-of-light-infer-relate.md).
- Trait selection and the trait-error leaf search (`next_solver/infer/{select,traits,errors}.rs`):
  the db-ty-port-infer-errors section of [speed-of-light.md](speed-of-light.md).
- Borrow checking one body (`crates/hir-ty-borrowck/`: dataflow, liveness, region inference,
  loans, Polonius): [speed-of-light-borrowck.md](speed-of-light-borrowck.md).
- Type lowering and item well-formedness (`lower.rs`, `lower/`, `generics.rs`, `variance.rs`,
  `crates/hir-ty-wfcheck/`): [speed-of-light-lower.md](speed-of-light-lower.md).
- Type inference of one body (`infer.rs`, `infer/`, `InferenceResult`'s columns,
  `infer/persist.rs`): [speed-of-light-infer.md](speed-of-light-infer.md).
