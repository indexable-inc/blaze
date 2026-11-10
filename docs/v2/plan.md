---
status: plan
summary: The plan for blaze, whole, on one page. The goal, the rules, the one design every part shares, the targets, and how the work is run; the ledger of objectives is plan/objectives.md.
date: 2026-11-09
---
# blaze: the plan

blaze is the Rust build system and compiler for coding agents: an edit comes back as running code
and a structured result in tens of milliseconds, on a warm daemon that every branch, pull request
and CI job shares. This page is the whole plan. Three more things hold the rest:

- **[plan/objectives.md](plan/objectives.md)**: every objective, the check that decides it, and
  where it stands. Work is picked from there and nowhere else.
- **[plan/bounds.md](plan/bounds.md)**: the speed-of-light bounds a program measures.
- **[plan/invariants.md](plan/invariants.md)**: every invariant the code states, with its check.
- **`plan/design/`**: one spec per design that is not fully built and needs more than a paragraph
  here. A spec is deleted, or becomes a `current` doc, when its design is built.

"v2" is only this directory's name. It merged into main on 2026-11-01; there is one blaze. The
415 lane notes this page replaces are in git at `fed1eb428d` (`git show fed1eb428d:docs/v2/plan.md`).

## Rules

AGENTS.md is the law for how code is written and landed. The plan adds what the product must be:

1. **Agents are the users.** Edits arrive as whole files and multi-file patches, from many agents
   at once. What counts is the time from a tool call to a structured result.
2. **Remove steps before speeding them up.** The fastest link is no link; the fastest rebuild
   compiles nothing. A step that stays is incremental per function.
3. **Within 10% of the bound.** A component has a least possible cost from arithmetic on its work
   and the hardware. One a program can measure is in [bounds.md](plan/bounds.md); more than 10%
   off is a bug.
4. **One of each.** One identity scheme, one query engine, one store, one scheduler, one memory
   budget, one span stream, one API. Two mechanisms for one job is a defect: merge them and ban
   the loser's name (AGENTS.md `one-concept-one-home`).
5. **Everything is ours.** Any crate, vendored dependency, format or interface changes when it
   makes blaze faster and keeps Rust's meaning.
6. **Deterministic.** The same inputs give the same bytes, whatever the cores, the order or the
   clock.
7. **It just works.** Real projects build and pass their own tests with no flags. rustc and cargo
   are references, not specs.
8. **Never take the machine down.** Work is admitted against a memory budget; what does not fit
   stops with an error that names it.

## The design

Every part is the same idea applied again: **a result is a pure function of content, keyed by the
hash of exactly what it read, computed once on one scheduler, kept in one store.** The parts below
are that idea at each layer, and each names the one thing the others must use.

### 1. Source is a Merkle tree

The daemon's source of truth is a content-addressed tree, not the disk. A **view** (a branch, a
pull request, a CI job, an agent's what-if patch) is a root hash; unchanged subtrees are shared.
An edit is an atomic transaction that makes a new root. What changed is the diff of two roots,
which skips equal subtrees by hash. Disk is one writer and one projection. While the daemon is
up the OS reports changes (fanotify on Linux, FSEvents on macOS, the USN journal on Windows);
after a restart the journals, or on Linux one parallel statx sweep, say what changed, and no file
is read whose metadata did not move.
Specs: [vfs](plan/design/vfs.md), [vfs-sync](plan/design/vfs-sync.md),
[restore](plan/design/restore.md).

### 2. Identity is content, never a name or a place

One scheme (`blaze_ids`, BLAKE3) names everything. An item has an interface hash, a layout hash,
a code hash and a deep hash. A compiled body's key holds what it does, never what it or its
callees are called or where they are written, so a rename, a move, a comment and a reformat
compile nothing. A key covers every input its value depends on and nothing else: no absolute
path, checkout, home or toolchain location.
Specs: [code-identity](plan/design/code-identity.md), [item-hash](plan/design/item-hash.md),
[name-free-identity](plan/design/name-free-identity.md), [caching](plan/design/caching.md).

### 3. One query engine, pull and push

`blaze-query` is the front end's only database: one concrete `&Db`, `Sync`, shared by reference,
every call static. A result is remembered with the fingerprints of what it read; it is reused
when those still match (early cutoff at interfaces, so a body edit reruns that body's queries
and nothing else). The few relations where checking on demand costs the whole graph (name
resolution, impl tables, trait candidates) are instead maintained on the write side by deltas, as
DBSP does. `blaze why` answers "why did this rerun" from the recorded reads, never by guessing.
Language semantics live only here.
Specs: [frontend](plan/design/frontend.md), [dbsp](plan/design/dbsp.md), [why](plan/design/why.md),
[kernel](plan/design/kernel.md).

### 4. Data is columns

IDs are `u32`. Syntax is a flat tree; HIR, MIR, types and the solver's tables are columns with
side tables for spans; lists are CSR; hot and cold fields are split; sizes are held by const
asserts. The same bytes are the in-memory form, the store's form and the wire's form, so nothing
is decoded on a hit and a restart maps the image instead of rebuilding it.
Spec: [data](plan/design/data.md).

### 5. One store

`blaze-cas` keeps every result under its key: plain when hot, packed when warm, compressed when
cold. It reads through its own buffer pool, not `mmap`: a reserved address range, explicit
reads on the IO driver in parallel, and eviction by advice, so a page fault never stalls a worker
and the pool's memory is counted in the one budget ([store-pool](plan/design/store-pool.md)).
Reads never lock. A commit is one atomic flip. The remote cache is the same
store off the critical path with the same keys, so a cold build is mostly a download, and a
second checkout's outputs are clones. Trust is by verification: a sampled share of hits is
recomputed and compared, and a model checker drives the real store through crashes.
Specs: [store](plan/design/store.md), [verify](plan/design/verify.md),
[storemodel](plan/design/storemodel.md), [clone-out](plan/design/clone-out.md),
[remote-cache](../remote-cache.md).

### 6. One scheduler, one memory budget

Every unit of work (a query, a codegen unit, a link stage, a test, a sandboxed process) is a task
on one work-stealing pool of fibers. A waiting task parks its fiber and frees its thread; equal
keys are computed once (single flight); order follows the critical path learned from history.
Every kernel wait goes through one deadline wait, every lock is a named lock that records who
waited behind whom, and every core is a sans-IO state machine with a thin driver (io_uring,
kqueue, IOCP, simulation), which is what makes deterministic simulation tests possible. Memory is
one pool run like a database buffer pool: every byte belongs to a running task's frame or to a
named cache, tasks are admitted by measured peak, and one evictor keeps the sum under the budget.
Specs: [scheduler](plan/design/scheduler.md), [fibers](plan/design/fibers.md),
[concurrency](plan/design/concurrency.md), [deadline-waits](plan/design/deadline-waits.md),
[named-locks](plan/design/named-locks.md), [memory](plan/design/memory.md),
[memory-budget](../memory-budget.md), [memory-pool](../memory-pool.md).

### 7. Code per function, and no link in the dev loop

A codegen unit is one function instance, keyed by its code identity; equal instances are one.
MIR is lowered once to LIR, blaze's own low-level IR (after layout, ABI and drops; before
instruction selection; columns), and every producer of code reads it: the stencils, LLVM, and
the wasm emitter. Cranelift IR is no longer an input of code generation.
A dev build compiles a function when it is first called (lazy), by copy-and-patch from stencils,
into the running process (no binary on disk, no link), and an edit patches the new code in place
and rolls it back if checks fail. Stencil code is the first tier, not the last: a function the
running program shows hot is recompiled by the optimizing tier in the background and patched in
the same way, so dev code is fast without any edit waiting for an optimizer. Dependencies are
compiled optimized once, shared by every checkout through the store, and arrive as one premapped
image. A release build goes
through blaze's pinned LLVM with per-function summaries, and blaze's own linker writes ELF,
Mach-O and COFF in parallel, in place. blaze never runs rustc; what still does is listed in
[no-rustc](../no-rustc.md).
Specs: [compile-time](plan/design/compile-time.md), [codegen](plan/design/codegen.md),
[icf](plan/design/icf.md), [polymorph](plan/design/polymorph.md),
[constspec](plan/design/constspec.md), [devirt](plan/design/devirt.md),
[heap-to-stack](plan/design/heap-to-stack.md), [field-order](plan/design/field-order.md),
[code-layout](plan/design/code-layout.md), [tail-calls](plan/design/tail-calls.md),
[toolchain-c](plan/design/toolchain-c.md).

### 8. User code is sandboxed, and its reads are its key

Build scripts and proc macros always run in a sandbox that records every read; those reads are
the result's key. Proc macros are wasm32-wasip1 modules under Wasmtime. A build script whose MIR
proves it pure is keyed without running. Tests run one process each from a fork server and are
selected by the code hashes they reached, so a change runs only the tests it can affect. Doctests
are built by blaze's own front end. One `blaze check` covers every configuration of a package.
Specs: [proc-macros](plan/design/proc-macros.md), [doctests](plan/design/doctests.md),
[multiconfig-check](plan/design/multiconfig-check.md), [lock-policy](plan/design/lock-policy.md).

### 9. One interface

The agent API is one versioned schema in deterministic CBOR over a Unix socket and HTTP; the CLI
is a client of it. Everything blaze says is data first (a `Report`: headline, rule, facts, fixes),
rendered for a terminal or handed to an agent as is. Diagnostics are judged by usefulness, with
cargo's JSON kept where tools parse it. CI is the same daemon: a webhook makes a view per pull
request, and the check run is the build and the tests that view reaches.
Specs: [structured-output](plan/design/structured-output.md), [ci](plan/design/ci.md).

### 10. One span stream

Every stretch of worker time and every wait is a span in one recorder (blaze's own, not
`tracing`). The stream is kept as Parquet history; `blaze explain`, `blaze debug sql`, `blaze
debug diff`, the live build view and the explorer all read that one stream.
Specs: [profiling](../profiling.md), [ix-prof-format](../ix-prof-format.md),
[no-tracing](plan/design/no-tracing.md), [profile-history](plan/design/profile-history.md),
[build-view](plan/design/build-view.md), [explore](plan/design/explore.md),
[explore-core](plan/design/explore-core.md), [explore-generic](plan/design/explore-generic.md),
[http3-sans-io](plan/design/http3-sans-io.md), [perf-history](plan/design/perf-history.md).

### 11. Proof by check, not by review

| claim | what decides it |
|---|---|
| a program means what the pinned nightly says | real projects build and pass their own test suites (`harness/v2/realprojects.py`, `corpus.py`) |
| an incremental build equals a build from nothing | the oracle (`harness/v2/oracle.py`), under- and over-invalidation both |
| output is deterministic | build twice, compare bytes (`harness/v2/determinism.py`) |
| it is fast enough | the budgets and bounds (`harness/v2/budgets.py`) |
| concurrent, durable or crash-recovering code is right | a deterministic simulation test with an invariant oracle and a planted bug |
| lock-free code is right | the model checker ([model-checker](plan/design/model-checker.md)) |
| unsafe code is sound | blaze's own UB checker on the MIR evaluator, and fuzzing ([unsafe](plan/design/unsafe.md)) |

**One statement, a ladder of evidence.** What the code promises is written once, in blaze, and
every kind of check reads that same text:

- a **contract** (`requires`, `ensures`) says what one call does ([contracts](plan/design/contracts.md));
- a **reference model** is a small, pure, sequential implementation of what a component means
  ("a store is a map from key to bytes"), an ordinary type in the crate;
- a **refinement** says the real component behaves as its model, or as another implementation
  (an incremental build as a build from nothing, `Frames` as `Fibers`, the simulated driver as a
  native one).

A statement then climbs as far as it needs to, unchanged:

| level | what checked it | says |
|---|---|---|
| tested | a property test generated from the contract or the refinement | no counterexample in N tries |
| simulated | the same, under the deterministic simulation with faults, crashes and interleavings, and a planted bug | none under adversarial schedules |
| model-checked | every interleaving to a bound | none up to the bound |
| proved | Lean, from the same contract and model ([formal-verification](plan/design/formal-verification.md)) | none |

The model is the object all four share: the simulation runs the real code against it, and it is
the first thing translated to Lean, where properties of the model are proved; the compiled Lean
model run against blaze-built code on drawn inputs is one more refinement. Proofs go to the
sans-IO cores, which the capability rules make provably free of IO, clock and globals; the
simulation covers the drivers around them and the binary as built. Durable, concurrent and
crash-recovering code is at least simulated; what must never be wrong (a key's hash, a format's
round trip) is proved. When two levels disagree, one of them has a bug, and that is a finding.

In the code, a statement is a doc comment opening `/// The invariant:`, `/// The refinement:` or
`/// The reference model:`. `harness/v2/invariants.py` collects them, with the level each has
reached, into [invariants.md](plan/invariants.md), and fails the gate when one has no planted-bug
control beside it or names a proof that is not there. So what blaze guarantees, and how strongly,
is read in one place; dropping or weakening a guarantee is a visible line in a pull request; and
generated code is judged against that list, not against itself.

How each harness is run is in [harness.md](../harness.md).

### 12. Extensions are opt-in and rest on few primitives

blaze's own language features are libraries on the language where they can be, and otherwise the
smallest general primitive, checked on MIR in one shared walk. They build on each other in this
order: [pure](plan/design/pure-fns.md), then [effects](plan/design/effects.md) and
[may-suspend](plan/design/may-suspend.md), then [linear](plan/design/linear.md) and
[contracts](plan/design/contracts.md), then verification ([verus](plan/design/verus.md),
[formal-verification](plan/design/formal-verification.md)). Beside them:
[comptime](plan/design/comptime.md), [arbitrary-ints](plan/design/arbitrary-ints.md),
[bits](plan/design/bits.md), [sealed-orphan](plan/design/sealed-orphan.md),
[specialization](plan/design/specialization.md), [borrowck-cursor](plan/design/borrowck-cursor.md)
and the lints [vecreport](plan/design/vecreport.md) and
[borrow-workarounds](plan/design/lint-borrow-workarounds.md). Further out: [future/](../future/).
blaze's own crates use each extension as soon as it exists.

### 13. Its own standard library

`bstd` is blaze's standard library, written from scratch on `core` and `alloc`: no ambient
authority (`main` receives an `Io`, and a function touches only what it is handed), sans-IO
underneath (one `Io` over io_uring, kqueue, IOCP and the simulation of section 6), and the one
lock, map, clock, wait and error shape the rules already name. A program on it is a deterministic
simulation test for free, and its signatures are its permissions. Every blaze crate moves onto
it; later it is the `std` of the blaze edition, where the sandbox of section 8 is derived from
what `main` hands out. Spec: [std](plan/design/std.md).

### 14. Its own edition

`edition = "blaze-2026.11"` is Rust with ten opt-in changes: handles that copy themselves, error
unions, view types, records and named arguments, contexts (`with`), effects declared at the
border, values that must be used up, contracts with pattern types, compile-time reflection, and `&uniq` as the spelling of the unique reference.
It removes lossy `as`, `async`, ambient IO, the global allocator and `static mut`. The lint group
`blaze::edition` gets a crate ready before the edition exists. Sections 12 and 13 are its parts.
Spec: [edition](plan/design/edition.md).

## How the work is run

The old way (70 agent windows, lanes that owned paths, request files between them, nothing
compiled until every lane said done) produced code nobody had run and notes nobody could read.
This replaces it.

**The ledger decides.** The ledger is data, `plan/objectives.json`: one row per objective, with
the outcome, the check that decides it as a command, the paths it touches, what it needs first,
and its status. `harness/v2/ledger.py` runs a row's check and writes the status with the commit
(`run`), says what to work on next without two tasks colliding (`pick`), and writes the table
people read, [objectives.md](plan/objectives.md) (`write`). Nobody types "done" and nobody argues
it. An objective without a check says which one is missing, and writing it is the first part of
the work; until then the row cannot be met.

**Every agent has one task.** A task is an objective id, or a named piece of one. An agent with no
task is not started; a task says its outcome and its check in one line each. The agent app's
sidebar lists tasks with their agents under them, so a person reads what is being done, not which
processes exist ([tasks](plan/design/tasks.md)).

**The loop**, run by one coordinator (the main agent of a session):

1. **Measure.** `ledger.py` `run` on main's exact tree, for the rows about to be worked on. Never
   start from a status someone remembered.
2. **Pick.** `ledger.py` `pick` gives rows that are ready and whose paths do not overlap, in the
   ledger's order; take fewer when one is large.
3. **Brief.** Each task gets the objective's row, the section of this page and the spec it
   belongs to, the paths it may touch, and what "done" prints. A row that needs a design gets
   its spec written first, by the same task that then builds it.
4. **Write.** Workers only write and commit, each in its own worktree. They build and run
   nothing. Use a worker only when the piece is independent and large enough to pay for its
   context; the coordinator does small pieces itself. Four workers is a lot.
5. **Integrate once.** The coordinator merges every worker's commits into one tree, runs one
   `blaze check` and one `blaze test`, then each row's check.
6. **Land** what passes, one pull request per objective, through `land.py`. What fails goes back
   to its worker once with the check's output; a second failure is written into the row as what
   blocks it, and the task stops.
7. **Ratchet.** The release train runs every met row again (`run` with `only: "met"`) and fails
   when one is no longer met, so nothing regresses silently. Then go to 1.

**Docs stay small.** This page, the ledger, the bounds and one spec per unbuilt design. No lane
notes, no pass reports, no indexes of indexes: what was done is the pull request and the commit; a
measurement worth keeping is a `record` under `fed1eb428d:docs/reports/`. A design that changes edits its
spec in place. When a section here stops being true, fix it in the same change.

## Settled

Decided; object with evidence, don't work around.

- One warm daemon per project; branches, pull requests and CI jobs are views, not forks. A VM
  fork is for tenants and untrusted whole jobs.
- No IDE and no MCP. The agent API is the interface.
- No GPU in the compiler; SIMD and huge pages are chosen at run time and every path is
  bit-identical.
- One native I/O driver per target, chosen by `cfg`; where it is unavailable blaze stops and
  says why.
- Stackful fibers, not async, inside the scheduler; no IO inside a job.
- The front end's algorithms may be redone wherever a faster design exists; the bar is behaviour,
  proved by real projects' tests and the oracle.
- `unsafe` only where it buys measured speed, behind a safe API, with `// SAFETY:`.
- Finished code only: no `todo!`, no `unimplemented!`, no fallback.
