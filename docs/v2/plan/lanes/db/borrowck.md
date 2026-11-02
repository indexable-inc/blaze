---
status: plan
summary: blaze v2's borrow checker (`crates/hir-ty-borrowck`) against rustc's `rustc_borrowck`, `rustc_mir_dataflow` and Polonius, where it matches them, where it beats them and what is left.
date: 2026-10-29
---
# blaze v2: the borrow checker's techniques (db-ty-borrowck)

Design notes for [the db lane](../db.md), as written on 2026-10-29. The code is
`crates/hir-ty-borrowck/`. Requests are in `v2/requests/db-ty-borrowck.md`.

## The benchmark

The benchmark is rustc's `rustc_borrowck` and `rustc_mir_dataflow`, plus Polonius. rustc's
techniques there are `DenseBitSet`/`ChunkedBitSet` gen/kill, a `WorkQueue` in reverse
postorder, `on_all_children_bits` over the move-path tree, `SparseIntervalMatrix` liveness and
SCC region inference.

## Where it matches rustc

It matches rustc's dataflow: bitsets of `u64` words, gen/kill composed once per block, a
worklist in reverse postorder, Tarjan SCCs with the universal regions as a bit matrix per SCC,
and interval sets of points.

## Where it beats rustc

- **Move paths numbered in preorder.** A subtree is a contiguous id range. A move or init sets
  or clears a word range, and "any descendant uninitialized" is a word scan. rustc visits every
  child bit by bit.
- **Block effects composed once.** Each block's effect is composed once into gen and kill
  words, and the fixpoint applies those. rustc applies every statement's effect again on each
  visit of a block. One statically dispatched function per statement builds the composition
  and drives the final walk.
- **One CSR per relation.** Every relation is one CSR built by an in-place counting sort, with
  two allocations and no per-node `Vec`. The relations are the CFG edges both ways, moves and
  inits per point, loans per local and point, the outlives graph and the Polonius edges.

The universal-region relation is Warshall's algorithm on a bit matrix (`n³/64` word ops).

## What is left

- A per-body bump arena for the scratch vectors.
- SIMD row unions, where the matrices are wide enough to pay.
