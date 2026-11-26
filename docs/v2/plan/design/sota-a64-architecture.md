---
status: plan
summary: Clean SOTA AArch64 compiler architecture eliminating duplicated ad-hoc passes: unified ValueInfo (KnownBits + ConstRange + dominator refinement), unified BlockFreq + Ext-TSP basic-block layout, AArch64 post-RA MachIR (copy-prop/anti-dep renaming, cross-ALU LDP/STP pairing, Apple M5 post-RA scheduler), and PGO-by-default export LTO + 16KB hot-page packing.
date: 2026-11-24
---
# SOTA AArch64 Compiler Architecture

## Problem & Why Current Code Is Replaced

Iterative micro-optimizations exposed four structural defects violating `one-concept-one-home` (Rule 30), `end-state-not-hill-climb` (Rule 66), and `pgo-by-default` (Rule 86):

1. **Three duplicated bit/range analyses** (`prop.rs` `BitAnalysis`, `egraph.rs` `compute_bool_01_vals`, `vectorize.rs` `BranchUpperBounds`) instead of one shared lattice.
2. **Two duplicated static-heuristic block placers** (`isel.rs::compute_hot_emit_layout` and `release_opts.rs::compute_block_order`) with no frequency model and no Ext-TSP objective.
3. **No post-RA AArch64 Machine IR** between `isel.rs` and binary encoding, forcing fragile pre-isel register guessing in `sched.rs` (which `sota/mod.rs` also skipped function-wide whenever any cold loop vectorized!) and preventing cross-ALU `ldp`/`stp` pairing and Apple M5 post-RA scheduling.
4. **Disconnected PGO in `blaze export`**: `export_lto.rs` passes `NoWeights` instead of `PgoSiteTable::estimate_static` + persisted cross-worktree `BPG1`, and `blaze-export/src/layout.rs` scatters hot kernels across 11 × 16KB pages when unranked.

## Chosen End State (4 Pillars, One Home Each)

1. **`blaze_lir::simplify::value_info::ValueInfo`**: Single SoA, non-recursive, explicit-allocator abstract interpreter combining `KnownBits { zeros, ones }` + unsigned `ConstRange { lo, hi }` with dominator-tree branch refinement (including XOR-range `(a ^ b) <u bound`, `Sub(x, c) < k`, bool conjuncts, and `Param` meet). Replaces and deletes the 3 ad-hoc copies in `prop.rs`, `egraph.rs`, and `vectorize.rs`.
2. **`blaze_emit::sota::layout::compute_block_layout`**: Single `BlockFreq` (Wu-Larus + loop-nest fixed-point + optional `Bpg2Profile` counts) + Newell-Pupyrev **Ext-TSP** basic-block placer for both AArch64 and x86-64. Feeds `edge_weight` directly into `isel` branch orientation and `regalloc` coalescing weights. Replaces and deletes `isel.rs::compute_hot_emit_layout` and `release_opts.rs::compute_block_order` / `hoist_multi_pred_br_latches`.
3. **`blaze_emit::sota::mach_a64`**: SoA AArch64 post-RA Machine IR (`MachFunc` / `MachBlock` / `MachInst`) running 4 provably-sound passes before final branch patching:
   - `mach_peephole_a64`: copy-prop, dead-mov removal, backward producer renaming, flag-setting `adds`/`subs`/`ands` fusion, and anti-dependency renaming + `csel` reorder eliminating loop-latch copies.
   - `mach_pair_ldp_stp`: cross-ALU non-aliasing 32b/64b/128b `ldp`/`stp` pairing.
   - `mach_sched_m5`: Apple M5 / Graviton 5 post-RA list scheduler (10-wide decode, 6 ALU, 3 LD/ST, 4 FP/NEON, preserving macro-op `cmp+b.cc` fusion and hoisting `ldr`/`mul`).
   - `mach_branch_opt`: fallthrough `b +4` elimination, branch threading, exact relocation/trap/patch offset remapping.
4. **PGO-by-Default Export (`export_lto.rs` + `blaze-export/src/layout.rs` + cycle-aware `regalloc.rs`)**: Replace `NoWeights` with `PgoSiteTable::estimate_static` merged with persisted cross-worktree `BPG1`, pass hot-body rank to `blaze-export`'s 16KB Ext-TSP packer so hot kernels pack into 1–2 contiguous 16KB iTLB pages, and model true parallel-copy cycle-swap cost in `regalloc::refine_gpr_moves`.

## Losing Alternatives

- Continuing to add peephole variants to monolithic `isel.rs` and pre-isel `sched.rs` (violates Rules 30 & 66; phase-ordering wars).
- Full greenfield rewrite of `isel.rs` before `mach_a64` exists (delays end-to-end verification across the 321 existing tests).

## Cost Budget & Incremental-First Compliance

- All passes operate strictly per function (or bounded demand-driven export LTO), `#![no_std]`, `#![forbid(unsafe_code)]`, explicit allocator `alloc`, SoA columns, O(N) / bounded worklists (Rule 18), deterministic output (Rule 25), zero daemon/reindex requirement across restarts or worktrees (Rule 60).
- Hardware target: Apple M5 Max (1 core) and 128-core Graviton 5 / EPYC (one code path), beating `rustc`/LLVM across all `blaze-zstd` modes (`Fast -1`, `Default -3`, `Decompress`, total `user` CPU) and C `zstd 1.5.7 --single-thread`.

## Verification Plan

- Unit tests + `verify()` + `eval::run_in` + `exhaustive_i8_validator` for `value_info`, `prop`, `egraph`, `vectorize`.
- Unit tests + `exec_leaf` hardware execution + `sota_matches_evaluator_across_150_random_lir_programs` for `layout`, `mach_a64`, `regalloc`, `isel`.
- Bit-exact RFC 8878 `zstd -dc` + byte-identical output to `rustc` on `corpus.rs`.
- Solo 10-pass interleaved shootout vs `rustc` and C `zstd 1.5.7 --single-thread`, all 6 workspace ratchets clean, and exact-tree Rule 68 `smoke` before merge.
