---
status: plan
summary: speed of light for hir-ty's glue around the next trait solver (fulfillment loop, deep and structural normalization, proof-tree inspection), with the arithmetic and what the code does to reach it.
date: 2026-10-29
---
# Speed of light: the solver glue in `next_solver/`

Files: `crates/hir-ty/src/next_solver/{solver,fulfill,normalize,structural_normalize,obligation_ctxt,inspect,format_proof_tree}.rs`.
Nothing here was run: costs are counted from the code, with the machine and unit costs of
[speed-of-light-infer-relate.md](speed-of-light-infer-relate.md) (L1 hit about 3 cycles, L2 about
16). The solver's own work (`vendor/rustc_next_trait_solver`, the global cache) is not counted
here. This glue only has to add nothing measurable on top of it.

## Fulfillment loop (`fulfill.rs`)

The work is one check per pending obligation per round. A stalled obligation costs
`goal_remains_stalled`: one root lookup per stalled variable in the union-find, which is about
2 L1 hits, so about 10 cycles for a typical 1 to 2 variables. With N pending and R rounds, the
bound is about `10 * N * R` cycles plus the solver's evaluations of the goals that did change.

How the code gets there:
- `Pending` keeps each field of an obligation in its own column (structure of arrays), so the
  stall check walks the `stalled_on` column alone.
- `retain` compacts every column in place, so a round allocates nothing.
- A goal the fast path proves is never stored.
- An error allocates only on the error path.

## Deep normalization (`normalize.rs`)

The common case is a value with no alias the solver may still normalize. `has_non_rigid_aliases`
answers it with one test of the cached flags (one L1 hit) and returns without folding. This is
rustc's rule, and the same early exit runs at every node of the vendored `NormalizationFolder`.
An alias that is not rigid costs one fresh variable and one root `Projection` goal. When the
global cache hits, that is a hash probe of about 50 to 100 cycles, which is the solver's cost and
not the glue's. Ambiguous goals are the only thing collected, so an unambiguous normalization
allocates nothing beyond the solver's own work.

## Structural normalization (`structural_normalize.rs`)

A term that is not a non-rigid alias returns after one kind test. An alias registers one
`Projection` obligation in the caller's fulfillment context, with no extra goal for the
alias-relate relation, which the nightly removed.

## Proof trees (`inspect.rs`, `format_proof_tree.rs`)

Proof trees are built only for diagnostics, overflow reports and the proof-tree dump, so they
are off the hot path. Each nested goal costs one uncached root evaluation. That is why
`instantiate_nested_goals_from` builds trees only for the sources the caller asks for:
`BestObligation` (errors.rs) only needs `Normalization` goals, so the other nested goals cost
only the instantiation of their canonical state.
