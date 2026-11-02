---
status: plan
summary: lane safety's design: hir-ty-ub (our own Miri over hir-ty-mir's evaluator), the evaluator's Machine hooks, fuzz targets, the model checker, and the unsafe audit.
date: 2026-10-29
---
# Lane safety: notes

The plan is [unsafe.md](../../design/unsafe.md). This file says how lane `safety` builds what
proves it. Miri's source cited here is `rust-lang/rust` at `ad3d0bc141a0`
(`src/tools/miri/`, `compiler/rustc_const_eval/src/interpret/`); paths below are relative to it.

## Who does what

| piece | paths | agent |
|---|---|---|
| crate root, shared ids, `Ub` | `crates/hir-ty-ub/{Cargo.toml,src/lib.rs,src/ids.rs,src/ub.rs}`, this folder | safety |
| memory: allocations, init, provenance, bounds, frees, leaks | `crates/hir-ty-ub/src/memory.rs`, `src/memory/` | safety-mem |
| Tree Borrows | `crates/hir-ty-ub/src/borrows.rs`, `src/borrows/` | safety-tb |
| data races, weak memory, scheduler, sync | `crates/hir-ty-ub/src/race.rs`, `src/race/` | safety-race |
| the `Machine` impl, validity and retag visitor, `run_test`, program tests | `crates/hir-ty-ub/src/check.rs`, `src/check/`, `crates/hir-ty-ub/tests/` | safety-check |
| fuzz targets and the coverage-guided engine | `fuzz/` (package `blaze-fuzzer`) | safety-fuzz |
| model checker, model tests | `crates/blaze-check/` ([model.md](model.md)), tests in owners' crates by request | safety-model |
| `// SAFETY:` and `forbid(unsafe_code)` audit | files of finished owners only | safety-audit |

## hir-ty-ub

### Shape

The evaluator owns the bytes; the checker owns everything else about them. The evaluator is
generic over a `Machine` (below); `Plain` is the one const eval and every run without `--ub`
use, a zero-sized type whose hooks are empty `#[inline(always)]` functions and whose
provenance and shadow types are `()`, so a build without the checker carries none of it.

`check::Checker` is the `Machine` of `--ub`. It composes:

- `memory::Memory`: allocations by address range (a sorted column of starts, binary search),
  each with its id, kind, size, align, liveness, an init mask (Miri's `InitMask`: run-length
  blocks) and a provenance map (Miri's `ProvenanceMap`: sorted `(offset, Prov)` per pointer, and
  bytewise fragments for partial pointers). It checks bounds, use after free, alignment,
  null and dangling pointers, frees (kind, size, align, double free, interior pointer),
  exposure for int-to-pointer casts, and leaks (heap memory not freed and not reachable from a
  global: Miri's leak check, `src/tools/miri/src/machine.rs` and
  `src/tools/miri/src/diagnostics.rs` `report_leaks`).
- `borrows::Borrows`: Tree Borrows per allocation, a port of
  `src/tools/miri/src/borrow_tracker/tree_borrows/` (`perms.rs`, `tree.rs`, `unimap.rs`,
  `foreign_access_skipping.rs`, `wildcard.rs`, `diagnostics.rs` for the history in reports) and
  of the protector bookkeeping of `src/tools/miri/src/borrow_tracker/mod.rs`.
- `race::Races`: `src/tools/miri/src/concurrency/` (`vector_clock.rs`, `data_race.rs`,
  `weak_memory.rs`, the sync objects of `sync.rs` reduced to what the evaluator's shims need,
  and `scheduler.rs`'s seeded choice of the next thread with Miri's preemption rate).
- `check::valid`: `compiler/rustc_const_eval/src/interpret/validity.rs` over hir-ty's types and
  layouts: run on every typed copy, it checks the value and retags each reference and `Box`
  in it with the copy's retag mode (as `ValidityVisitor::check_safe_pointer` calls
  `M::retag_ptr_value`), and resets padding to uninitialized and provenance on non-pointer
  bytes (`reset_provenance_and_padding`).

### Retags without Retag statements

blaze's MIR has no `Retag` statements, and needs none: current Miri retags inside the typed
copy, not at statements (`interpret/step.rs` `eval_rvalue_into_place`, `interpret/call.rs`
argument passing, `interpret/validity.rs` line ~623). The evaluator says the mode of each typed
copy and the checker does the rest:

| the copy | mode |
|---|---|
| `Assign(_, Ref(Mut { kind: TwoPhasedBorrow }, _))` | `TwoPhase` |
| any other `Assign(_, Ref(..))` to a reference type | `Default` |
| `Assign(_, Ref(..))` to a raw pointer type (`&raw`) | `Raw` (Tree Borrows: no retag) |
| every other `Assign` (`Use`, `Cast`, `Aggregate`, ...) | `Default` |
| a call's arguments into the callee's locals | `FnEntry` (protectors) |
| a call's return value into its destination | `Default` |

rustc's `WithRetag::No` marks only copies its own MIR passes add (deref temporaries, match
branch simplification), which blaze's MIR doesn't have.

### Errors

A hook that finds UB records it in the checker and returns `Err(Stop)`; the evaluator returns
`MirEvalError::MachineStop` (requested of db-ty, who owns `hir-ty`'s `MirEvalError`) wrapped in
its `InFunction` stack, and `check::run_test` turns that and the checker's record into a
report: the UB with Miri's wording, the thread, the statement, the call stack, and for an
aliasing error the history of the tags involved (Tree Borrows' `diagnostics.rs`).

## The evaluator's `Machine`

The contract is the trait as landed in `crates/hir-ty-mir/src/mir/eval/machine.rs` (codegen-mir
owns it; lane safety proposed it, in `v2/requests/codegen.md` "From safety" S1, and
safety-check's C3 and C7 in `v2/requests/safety.md` refined it). `check::Checker` implements it.
The calls, by what the checker sees:

- memory: `alloc` (returns the root provenance), `free`, `root`, `read`, `write`, `copy`,
  `require_init` (on the machine, so it can say what failed), `offset`, `expose`,
  `from_exposed`;
- atomics: `read`/`write` with `Access::Atomic`, then right after `atomic_load` (the value
  under weak memory) or `atomic_store` (with the bytes it overwrote), `fence`; a failed
  compare-exchange reads the latest store;
- values: `typed` after every typed copy, by the mode table above;
- frames: `call` before the arguments are copied, `protect` for in-place arguments and the
  return place (Miri's `protect_in_place_function_argument`), `ret` before the return value is
  copied out;
- threads: `spawn`, `schedule` (at each terminator and whenever the running thread blocks),
  `join`, `detach`, `exit`, `yield_now`, `thread_name`, `release`/`acquire` on a sync object's
  address;
- `step` before each statement and terminator (the site for reports), `finish` when the main
  thread returns.

The evaluator's side: `Evaluator<'db, M: Machine<'db> = Plain>`, provenance on every address
from a place to an access, an `M::Shadow` beside every value held outside memory, real threads
(`pthread_*`, futexes, `__ulock_*`, `sched_yield`), and `run_function::<M>` to run a `#[test]`.

## Fuzzing (`fuzz/`)

One target per parser of untrusted bytes, written against the parser's public API, each with
a seed corpus and the invariants it checks (`fuzz/README.md` lists them). The engine is ours:
SanitizerCoverage's inline 8-bit counters and PC table (`__sanitizer_cov_8bit_counters_init`,
`__sanitizer_cov_pcs_init`) and trace-cmp callbacks in one unsafe module, a corpus kept by new
coverage buckets, libFuzzer's mutations, and minimization. A target built without coverage
stops with an error naming the flag it needs (no-fallback); the seed corpora also run as
plain tests.

## Model checking (`crates/blaze-check/`)

A loom-style explorer: shim atomics and threads, every interleaving to a preemption bound by
dynamic partial-order reduction, and the C11 orderings by a store history per atomic. It is a
leaf crate rather than `blaze-graph/src/check/`: `blaze-reclaim` and the lower crates under
test can depend on it without a cycle. Each lock-free structure in `blaze-ids`, `intern`,
`blaze-query`, `blaze-cas`, `blaze-graph` and `blaze-reclaim` gets a model test against a
sequential model. Design and comparison with loom and shuttle: [model.md](model.md).

## Benchmarks this matches

- Miri itself for the checker's rules (the port is rule for rule), and Miri's speed as the bar
  to beat: Miri pays a `RefCell` and a `BTreeMap` lookup per access; ours is a flat column per
  allocation and one binary search per access.
- libFuzzer and AFL++ for the engine (8-bit counters, value profile, corpus minimization).
- loom and shuttle for the explorer (loom's bounded DPOR, shuttle's randomized PCT).
