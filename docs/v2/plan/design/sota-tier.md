---
status: plan
summary: Experimental non-LLVM SOTA optimization tier (`TierName::Sota`) over Blaze LIR: Acyclic E-Graphs (`aegraph`) + ISLE-style bottom-up DAG ISEL + Chordal-SSA Register Coloring + Dynamic PGO/Devirt, targeting LLVM -O2/-O3 runtime speed at 10-30x faster per-core compile speed and per-function incremental caching.
date: 2026-11-22
---
# Experimental SOTA Non-LLVM Optimization Tier (`TierName::Sota`)

## Goal
Provide an experimental optimizing tier beyond copy-and-patch stencils (`TierName::Fast`) that **does not invoke LLVM**, runs **10–30x faster per core** than LLVM `-O2/-O3`, preserves **per-function incremental caching and shape memoization**, and matches or beats static LLVM `-O3` on hot JIT workloads via runtime PGO + speculative devirtualization.

## Why Not LLVM for the Hot JIT / Fast-Incremental Tier?
1. **Pointer-chasing mutable IR**: LLVM's 80–120 sequential passes chase heap pointers (`Instruction*`/`Use*`) at ~5–15 MB/s per core (estimate) and suffer from pass-ordering phase interactions.
2. **Coarse incremental boundary**: LLVM optimizes whole modules/CGUs; even ThinLTO cannot early-cutoff individual small functions cleanly across a single-edit loop.
3. **Static-only assumptions**: Static LLVM cannot specialize hot indirect calls (`dyn Trait` / function pointers) or outline cold panic paths using live JIT execution counters without an extra PGO rebuild cycle.

## Architecture: Four Linear, Content-Addressable Stages over `blaze-lir`

| Stage | Algorithm | Complexity | Incremental Property |
|---|---|---|---|
| **1. Cross-fn PGO + ThinLTO** | Bottom-up Tarjan SCC inlining (`blaze-lir::lto`) + decayed `BPG1` PGO weights (`blaze-lir::pgo`) + speculative devirt | O(V + E) | Per-function `FnSummary` early cutoff |
| **2. Mid-End (`aegraph`)** | Dominator-scoped Acyclic E-Graph (`blaze-lir::egraph`): simultaneous GVN, commutative canonicalization, algebraic rewrites, strength reduction, exact constant folding, bounds-check elimination, LICM | O(N) single dom-tree pass | Pure, deterministic, sans-IO; canonical hash caches across edits |
| **3. Instruction Selection** | Deterministic bottom-up DAG/tree pattern rewriter (ISLE-style tablegen) lowering directly to AArch64 (`madd`, `csel`, shifted operands, `ldp`/`stp`, NEON) and x86-64 (`lea`, BMI2, AVX2) | O(N) | Target-parameterized template blob; shares `Shape::of` memo across monomorphizations |
| **4. Register Allocation** | Chordal-SSA Coloring (`blaze-lir::egraph::plan_ssa_regalloc` -> Braun-Hack SSA tree-scan + PGO-weighted live-range splitting + `phi` coalescing) | O(N) | Zero global module state; cold error paths spill before hot loops |

## Where It Can Beat Static LLVM `-O3`
- **Live JIT PGO + Speculative Devirtualization**: Because Blaze already collects `SampleRing` / `BPG1` counts during Tier 1 (stencils), Tier 2 (`Sota`) promotes only the hottest ~2–5% of functions with exact branch probabilities and monomorphic/polymorphic call-target guards (`if callee == expected { inlined } else { slow_call }`).
- **Rust-Native Alias & Range Facts**: Direct consumption of `&mut` exclusivity, enum niche ranges, and slice bounds (`i < slice.len()`) before lowering removes bounds checks and redundant loads in one pass without LLVM metadata loss.
- **Hot/Cold Block Splitting**: Cold panic/format/unwind blocks are split out of hot I-cache lines automatically using `BPG1` weights.

## Incremental Speed Advantage
- **Tiered Compilation**: Cold/edit path compiles in **Tier 1 (Fast stencils)** in microseconds; background threads tier-up hot functions into **Tier 2 (`Sota`)** asynchronously.
- **Shape-Memoized Optimized Code**: Two generic instantiations (`Vec<A>::push`, `Vec<B>::push`) with identical post-`aegraph` shapes compile once and share a single optimized machine-code record.
- **Early Cutoff**: If an edit changes source syntax or comments or simplifies to the same `aegraph` canonical form + `FnSummary`, downstream callers are untouched.

## Unified Feedback Loop (Loops + PGO + Opportunistic JIT Inlining + E-Graph + RegAlloc)

Rather than LLVM's fixed static pass pipeline, `TierName::Sota` runs an **iterative, profile-and-pressure-coupled feedback loop**:

```
  Tier 1 (Stencils) live counters / BPG1 PGO profile
           │
           ▼
  ┌─────────────────────────────────────────────────────────────────────┐
  │ 1. Natural Loop + Induction-Var + LICM Analysis (`analyze_loops`)   │
  │    - discovers natural loops, nesting depth (4x/level), LICM ops,   │
  │      and basic induction variables (foundation for BCE / LSR / SLP) │
  │                           │                                         │
  │                           ▼                                         │
  │ 2. PGO + Loop + RegPressure Opportunistic Inliner                   │
  │    (`plan_opportunistic_inlining`)                                  │
  │    - amplifies callee budget inside hot loops (up to 160 insts)     │
  │    - flags hot loop `CallIndirect` for speculative devirtualization │
  │    - throttles budget back when GPR headroom nears spill threshold  │
  │                           │                                         │
  │                           ▼                                         │
  │ 3. PGO-Guided Acyclic E-Graph (`aegraph_optimize` + `propagate` +   │
  │    `dead`) — single O(N) dom-tree pass:                             │
  │    - hot loops: latency-minimizing extraction + LICM + strength red │
  │    - cold blocks: code-size & register-pressure minimizing          │
  │                           │                                         │
  │                           ▼                                         │
  │ 4. Chordal-SSA Register Pressure (`plan_ssa_regalloc`)              │
  │    - exact peak GPR/FPR chromatic pressure feeds directly back into │
  │      step 2 so inlining never blows hot-loop registers into spills  │
  └─────────────────────────────────────────────────────────────────────┘
           │ repeats until fixed point (`sota_optimize_fixpoint`), then:
           ▼
  ISLE-style DAG ISEL (madd/csel/ldp/NEON) + Braun-Hack SSA Coloring
```

## Implemented Foundation
- `crates/blaze-lir/src/simplify/egraph.rs` (`blaze_lir::egraph`):
  - `aegraph_optimize`: Dominator-scoped hash-consed GVN, commutative canonicalization, algebraic identities, strength-reduction analysis, natural loop / LICM / induction-var discovery, exact constant folding, verified against `blaze_lir::eval`.
  - `plan_ssa_regalloc`: Chordal-SSA peak GPR/FPR chromatic pressure & spill estimator (`RegBudget::AARCH64` / `RegBudget::X86_64`) + cycle latency model.
  - `plan_opportunistic_inlining` & `sota_optimize_fixpoint`: PGO × loop-depth × register-headroom opportunistic JIT inlining & speculative devirtualization feedback loop.
- `crates/blaze-lir/src/{pgo.rs, lto.rs, summary.rs}`: Cross-run decayed `BPG1` PGO profile, bottom-up Tarjan SCC ThinLTO, and JIT tier-up planner.

## Next Milestones (Experimental `TierName::Sota`)
1. **E-Graph Strength-Reduction & Reassociation Materializer**: emit `shl`/`lshr`/`and` replacements directly in `aegraph_optimize` (`x * 2^k -> x << k`, `(x + c1) + c2 -> x + (c1 + c2)`), plus loop-invariant hoisting via dominator depth.
2. **ISLE-Style AArch64 DAG Selector (`blaze-emit/src/sota/`)**: fuse `add + shl`, `mul + add -> madd`, `icmp + select -> csel`, `load/store` offset addressing modes, and NEON 128-bit SLP vectorization.
3. **Chordal-SSA Coloring Emitter**: allocate directly from `SsaRegPlan` over `ARM64` / `X86_64_SYSV` registers (`blaze-tier::regs`) with PGO-weighted cold-block spilling and `bzcode08` output checked by the tier equality harness (`tier_tests.rs`).
