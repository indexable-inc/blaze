---
status: plan
summary: lane requests' speed of light: the bound each component in its paths (root Cargo.toml profiles, ide-db's batch runner, hir-ide's const macros and type display) has, the arithmetic, and what stands between it and 90%.
date: 2026-10-29
---
# Lane requests: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Lane `requests` applies other lanes' requests for the
coordinator, and its paths are the root `Cargo.toml`, `crates/hir-ide/`, `crates/ide-db/` and
`crates/test-fixture/`. The last three are removed at integration
([crates.md](../../reference/crates.md)), so most of their bounds are kept only until then. Figures
are for one core of the target machines, as in [hardware.md](../../design/hardware.md): about
3.5 GHz, an L1 hit in 4 to 5 cycles, L2 in about 14, a cache line moved between cores in about
100 cycles, and about 2 instructions per cycle for branchy code. Nothing has been measured yet:
nothing compiles until every lane is done, and `proof`'s budgets measure each bound once it does.

## The profiles blaze ships with (root `Cargo.toml`)

Every other component's bound assumes the machine code a good compiler makes. The profile sets
how close blaze's own binary comes to that, as a constant factor on every component.

- Bound: whole-program optimization of blaze: inlining across crates, code laid out by its
  profile (PGO), and hot paths packed by BOLT.
- Measured: thin LTO takes 4 to 5% off an edit's work in the daemon (the comment above
  `[profile.dist]`). PGO typically gives 10 to 15% more on compiler workloads, and BOLT a few
  percent on top.
- Design: `dist`, the profile that ships, has thin LTO and one codegen unit per crate
  (`codegen-units = 1`, so LLVM sees each crate whole), and is built with PGO and BOLT by the
  release train (fastcompile-sched 5, the coordinator's). `release`, which the harnesses and CI
  build after every change, has neither, so that rebuilding blaze stays fast.
- What stands in the way: a budget measured on a `release` binary already spends half of its 10%
  on the profile. `proof`'s budgets for this bar must run the `dist` binary, PGO and BOLT
  included, or say which profile they measured.
- `panic = "abort"` would remove unwind tables and landing pads, a few percent of code size on
  hot paths. It can't be set yet: salsa's cancellation still unwinds (frontend.md item 9, lane
  db), and the daemon catches a panicking query. Once cancellation returns `Err(Cancelled)`, that
  reason goes, and `dist` should then abort on panic.

## A batch of queries on the pool: `ide-db` `RootDatabase::run_all`

Work: run `count` independent tasks against the database on the worker pool, and wait for all
of them.

- Bound: the larger of the tasks' total time divided by the pool's threads and the longest
  task, plus the dispatch. Dispatch is one job pushed near the caller's group and one completion
  per task: about two cache lines that change cores, about 200 cycles (60 ns) per task. That is
  well under 10% of any task worth sending to the pool (a query of a few µs or more).
- Design: `base_db::run_on_pool` (lane db's) on the pool `set_pool` gave. It allocates nothing
  per task (one `Vec` of indices per round). Its two `Mutex`es are touched only when a task
  panics or must run again, so the common path takes no lock. The caller pump-waits, so its own
  thread runs tasks while it waits instead of sleeping.
- What stands in the way: a task that asks for a query a task below it on the same thread is
  running (`salsa::Cancelled::WouldWaitOnItself`) runs again in another round, after every task
  of this round has ended. Each such task can add one round's longest task to the critical
  path. What removes it is parking the task on the query it waits for and resuming it when that
  query ends (usercode-concurrency 5, `pump::wait_until`). That is the plane's and db's to build,
  and `run_on_pool` takes it with no change here.

## Const macros: `hir-ide` `const_macro.rs`

Work: expand a const macro, a proc macro whose body is a function of the analysis graph's macro
crate.

- Bound: the macro function's own work run as native code, plus moving its input and output.
  The input is rendered to text (or handed over as a flat tree) and the output is lexed into
  tokens. Each costs about a cycle per byte, about 0.3 ns per byte, so a 10 KB expansion moves
  in a few µs.
- Today: `call_const_macro` runs the function in blaze's MIR evaluator (`hir-ty-mir`). A MIR
  interpreter runs a program 50 to 500 times slower than native code (Miri-class interpreters sit
  at the slow end; a direct interpreter over MIR locals near the fast end). So expanding is far
  outside 10% of its bound for any macro that does real work. A macro that does little more than
  copy its input is limited by the input and output instead, and comes close.
- What stands in the way: the function is interpreted, not compiled. What removes it is
  compiling the macro crate with the dev tier (Cranelift or stencils, lane fastcompile) and
  calling the function in this process, through the in-process loader (fastcompile-inproc).
  Inputs are passed as `&str` or a pointer to the flat tree, as they are now, and the output is
  read back as text. The evaluator then stays only for `const` evaluation proper.
  Until the dev tier runs in process, this is the one component here more than 10% off its bound
  by design. It is a bug against the bar, owned with fastcompile, and moves with const macros to
  their own crate at integration.
- Smaller: a `&str` parameter gets its input through `TopSubtree::to_string`, which renders the
  tokens to text again. For an input that is the call's source unchanged, the bytes are already
  in the file, and a borrowed slice of the file saves the render, about 0.3 ns a byte.

## Rendering a type: `hir-ide` `display.rs`

Work: print a type, a const or a bound for a diagnostic, a hover or `blaze doc`.

- Bound: the output's bytes, about a cycle each, plus one visit per node of the type and one
  memoized lookup per name it prints (a signature query, about 20 to 40 ns from cache). A
  50-character type with 5 names prints in about 0.3 µs.
- Where it runs: only when text for a person is made: diagnostics, hovers and docs. A build that
  succeeds renders no type, so this is never on a build's critical path.
- What stands in the way:
  - Each piece goes through `dyn HirWrite`, an indirect call per piece. That is a few ns per
    piece against about a ns per byte of output, so within 2× of the bound for short pieces and
    close to it for names.
  - `write_projection` scans every predicate of the parameter's item for each projection it
    prints, to find bounds on that projection. The cost grows with the predicates in scope
    rather than with the output. The fix is to index the item's predicates by self type once, a
    query of lane db's. It is worth doing only if `proof` measures diagnostics rendering as a
    visible part of an edit, which it is not by design.

## `hir-ide`'s match and unsafety checks, and `test-fixture`

- The match check (`diagnostics/match_check/`) and the unsafety check (`diagnostics/unsafe_check.rs`)
  move to `hir-ty-matchck` and `hir-ty-unsafeck` (db's request 9). Their bounds belong to those
  crates' notes, and the checks here are not kept up to their bar.
- `test-fixture` builds databases for tests. It runs in no build blaze does, so it has no bound
  under this bar.
