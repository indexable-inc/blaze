---
status: plan
summary: The plan for blaze, whole, on one page. The goal, the rules, the one design every part shares, the targets, and how the work is run; decisions are the rows of plan/decisions.json.
date: 2026-11-26
---
# blaze: the plan

blaze is the Rust build system and compiler for coding agents: an edit comes back as running code
and a structured result in tens of milliseconds, from records every checkout on the machine
shares; a command answers and exits ([bottom](plan/design/bottom.md)). This page is the whole plan. Four more things hold the rest:

- **[plan/decisions.json](plan/decisions.json)**: every decision, one row each, with what was
  recommended, why, and where it stands. Work is picked from its rows and the specs below.
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
4. **One of each.** One identity scheme, one record form, one store, one scheduler, one memory
   measure, one span stream, one API. Two mechanisms for one job is a defect: merge them and ban
   the loser's name (AGENTS.md `one-concept-one-home`).
5. **Everything is ours.** Any crate, vendored dependency, format or interface changes when it
   makes blaze faster and keeps Rust's meaning.
6. **Deterministic.** The same inputs give the same bytes, whatever the cores, the order or the
   clock.
7. **It just works.** Real projects build and pass their own tests with no flags. rustc and cargo
   are references, not specs.
8. **Never take the machine down.** Footprint is the measure and the kernel's limit the ceiling;
   what does not fit stops with an error that names it.

## The design

Every part is the same idea applied again: **a result is a pure function of content, keyed by the
hash of exactly what it read, computed once on one scheduler, kept in one store.** The parts below
are that idea at each layer, and each names the one thing the others must use.

### 1. Source is a Merkle tree

The source of truth is a content-addressed tree, not the disk. A **view** (a branch, a
pull request, a CI job, an agent's what-if patch) is a root hash; unchanged subtrees are shared.
An edit is an atomic transaction that makes a new root. What changed is the diff of two roots,
which skips equal subtrees by hash. Disk is one writer and one projection. A command's stat
sweep against one small table says what changed, and no file is read whose metadata did not
move. Not built.

### 2. Identity is content, never a name or a place

One scheme (`blaze_ids`, BLAKE3) names everything. An item has an interface hash, a layout hash,
a code hash and a deep hash. A compiled body's key holds what it does, never what it or its
callees are called or where they are written, so a rename, a move, a comment and a reformat
compile nothing. A key covers every input its value depends on and nothing else: no absolute
path, checkout, home or toolchain location.
Specs: [bottom](plan/design/bottom.md) (section 3), [caches](plan/design/caches.md).

### 3. Records pulled at item grain

A question about one item reads or makes one small record keyed from text; a record is valid by
its key or by its read list, so a body edit reruns that body's tasks and nothing else. `blaze
why` answers "why did this rerun" from the recorded reads, never by guessing. Language semantics
live only in the front end. Specs: [bottom](plan/design/bottom.md),
[end-state](plan/design/end-state.md).

### 4. Data is columns

IDs are `u32`. Syntax is a flat tree; HIR, MIR, types and the solver's tables are columns with
side tables for spans; lists are CSR; hot and cold fields are split; sizes are held by const
asserts. The same bytes are the in-memory form, the store's form and the wire's form, so nothing
is decoded on a hit and a restart maps the image instead of rebuilding it.
Specs: [bottom](plan/design/bottom.md) (section 13), [columns](plan/design/columns.md).

### 5. One store

`blaze-cas` keeps every result under its key as a record written once and read in place: a
small one by `pread` into the task's frame, a large plain array mapped read-only. The kernel's
page cache is the only cache; there is no buffer pool of blaze's own.
Reads never lock. A commit is one atomic flip. A new checkout is served by the machine's own
content-deduplicated store, so its outputs are clones. A remote cache is supported and off unless
chosen: publishing and fetching records with the same keys is built, and nothing is designed
around it. Trust is by verification: a sampled share of hits is
recomputed and compared, and a crash simulation drives the real store through power cuts and
failed syncs.
Specs: [bottom](plan/design/bottom.md) (section 10), [store-next](plan/design/store-next.md),
[remote-cache](../remote-cache.md).

### 6. One scheduler, one memory measure

Every unit of work (a query, a codegen unit, a link stage, a test, a sandboxed process) is a task
on one work-stealing pool of threads. A task that lacks a fact returns `Need`, parks, and resumes
when the producers of what it read have merged; nothing starts over. Equal keys are computed
once (single flight); order follows the critical path learned from history.
Every kernel wait goes through one deadline wait, every lock is a named lock that records who
waited behind whom, and every core is a sans-IO state machine with a thin driver (io_uring,
kqueue, simulation; Windows later), which is what makes deterministic simulation tests possible. Every
byte belongs to a running task's frame or is a record; footprint is measured, and nothing admits
work by memory.
Specs: [parallel](plan/design/parallel.md), [end-state](plan/design/end-state.md) (D157, D161,
D179), [deadline-waits](plan/design/deadline-waits.md),
[named-locks](plan/design/named-locks.md), [memory](plan/design/memory.md).

### 7. Code per function, and no link in the dev loop

A codegen unit is one function instance, keyed by its code identity; equal instances are one.
MIR is lowered once to LIR, blaze's own low-level IR (after layout, ABI and drops; before
instruction selection; columns), and every producer of code reads it: the template tier, LLVM,
and the wasm emitter. Cranelift IR is not an input of code generation.
A dev build compiles a function when it is first called (lazy), by the template tier
(copy-and-patch with registers, D195), into the running process (no binary on disk, no link), and
an edit patches the new code in place and rolls it back if checks fail. Template code is the
first tier, not the last: a function the running program shows hot is recompiled by the
optimizing tier in the background and patched in the same way, so dev code is fast without any edit waiting for an optimizer. Dependencies are
compiled optimized once, shared by every checkout through the store, and arrive as one premapped
image. A release build goes
through blaze's pinned LLVM with per-function summaries, and blaze's own linker writes ELF
and Mach-O in parallel (`blaze build`). blaze never runs rustc; what still does is listed in
[no-rustc](../no-rustc.md).
Specs: [backend-next](plan/design/backend-next.md), [codegen](plan/design/codegen.md),
[icf](plan/design/icf.md), [polymorph](plan/design/polymorph.md),
[constspec](plan/design/constspec.md), [devirt](plan/design/devirt.md),
[heap-to-stack](plan/design/heap-to-stack.md),
[sota-a64-architecture](plan/design/sota-a64-architecture.md), [c](plan/design/c.md).

### 8. User code is sandboxed, and its reads are its key

Build scripts and proc macros always run in a sandbox that records every read; those reads are
the result's key. blaze compiles proc-macro crates and pure-Rust build scripts itself to wasm64
(memory64) modules and runs them in-process under Wasmtime; native tools and build scripts that
link C go to the hypervisor micro-VM sandbox. A build script whose MIR
proves it pure is keyed without running. Tests run one process each from a fork server and are
selected by the code hashes they reached, so a change runs only the tests it can affect. Doctests
are built by blaze's own front end. One `blaze check` covers every configuration of a package.
Specs: [proc-macros](plan/design/proc-macros.md), [doctests](plan/design/doctests.md),
[conditions](plan/design/conditions.md), [lock-policy](plan/design/lock-policy.md).

### 9. One interface

Everything blaze says is data first (a `Report`: headline, rule, facts, fixes),
rendered for a terminal or handed to an agent as is. Diagnostics are judged by usefulness, with
cargo's JSON kept where tools parse it.
Spec: [structured-output](plan/design/structured-output.md).

### 10. One span stream

Every stretch of worker time and every wait is a span in one recorder (blaze-trace, blaze's own,
not `tracing`), and what a run keeps is bstd's one telemetry report, which every reader reads.
Spec: [end-state](plan/design/end-state.md), D178.

### 11. Proof by check, not by review

| claim | what decides it |
|---|---|
| a program means what the pinned nightly says | real projects build and pass their own test suites |
| an incremental build equals a build from nothing | an oracle, under- and over-invalidation both |
| output is deterministic | build twice, compare bytes |
| it is fast enough | the bounds in [bounds.md](plan/bounds.md) |
| concurrent, durable, crash-recovering, billing or deleting code is right | a deterministic simulation test with an invariant oracle and a planted bug |
| lock-free code is right | the model checker ([model-checker](plan/design/model-checker.md)) |
| unsafe code is sound | blaze's own UB checker on the MIR evaluator, and fuzzing ([unsafe](plan/design/unsafe.md)) |

**One statement, a ladder of evidence.** What the code promises is written once, in blaze, and
every kind of check reads that same text:

- a **contract** (`requires`, `ensures`) says what one call does ([contracts](plan/design/contracts.md));
- a **reference model** is a small, pure, sequential implementation of what a component means
  ("a store is a map from key to bytes"), an ordinary type in the crate;
- a **refinement** says the real component behaves as its model, or as another implementation
  (an incremental build as a build from nothing, the simulated driver as a
  native one).

A statement then climbs as far as it needs to, unchanged:

| level | what checked it | says |
|---|---|---|
| tested | a property test generated from the contract or the refinement | no counterexample in N tries |
| simulated | the same, under the deterministic simulation with faults, crashes and interleavings, and a planted bug | none under adversarial schedules |
| model-checked | every interleaving to a bound | none up to the bound |
| proved | Lean, from the same contract and model ([formal-verification](plan/design/formal-verification.md)), on top of the levels below | none |

The model is the object all four share: the simulation runs the real code against it, and it is
the first thing translated to Lean, where properties of the model are proved; the compiled Lean
model run against blaze-built code on drawn inputs is one more refinement. Proofs go to the
sans-IO cores, which the capability rules make provably free of IO, clock and globals; the
simulation covers the drivers around them and the binary as built. Concurrent, durable,
crash-recovering, billing and deleting code is always simulated; a Lean proof is the top rung of
extra evidence and never replaces the simulation test. What must never be wrong (a key's hash, a
format's round trip) is also proved. When two levels disagree, one of them has a bug, and that is a finding.

In the code, a statement is a doc comment opening `/// The invariant:`, `/// The refinement:` or
`/// The reference model:`. `scripts/src/bin/invariants.rs` collects them, with the level each
has reached, into [invariants.md](plan/invariants.md), and fails the gate when one has no
planted-bug control beside it or names a proof that is not there. So what blaze guarantees, and how strongly,
is read in one place; dropping or weakening a guarantee is a visible line in a pull request; and
generated code is judged against that list, not against itself.

How each harness is run is in [harness.md](../harness.md).

### 12. Extensions are opt-in and rest on few primitives

blaze's own language features are libraries on the language where they can be, and otherwise the
smallest general primitive, checked on MIR in one shared walk. They build on each other in this
order: pure, then [effects](plan/design/effects.md) (purity is its first part) and
may-suspend ([extensions](plan/design/extensions.md)), then [linear](plan/design/linear.md) and
[contracts](plan/design/contracts.md), then verification ([verus](plan/design/verus.md),
[formal-verification](plan/design/formal-verification.md)). Beside them:
[comptime](plan/design/comptime.md), [arbitrary-ints](plan/design/arbitrary-ints.md),
[bits](plan/design/bits.md), [sealed-orphan](plan/design/sealed-orphan.md),
[specialization](plan/design/specialization.md), the borrow check's [flow](plan/design/flow.md)
and the lints [vecreport](plan/design/vecreport.md) and
[borrow-workarounds](plan/design/lint-borrow-workarounds.md). Further out: [future/](../future/).
blaze's own crates use each extension as soon as it exists.

### 13. Its own standard library

`bstd` is blaze's standard library, written from scratch on `core`: blaze's crates are `no_std`
without `alloc`, and no global allocator is reachable. No ambient authority (`main` names each
capability it uses, such as `out: Out` or `args: Args`, and a function touches only what it is
handed), sans-IO
underneath (one `Io` over io_uring, kqueue and the simulation of section 6), and the one
lock, map, clock, wait and error shape the rules already name. Context and memory are explicit
too: each task has a `Cx` (deadline, cancellation, span, frame allocator) passed as a parameter,
and there is no global heap, so running out of memory fails one task and never the process. A program on it is a deterministic
simulation test for free, and its signatures are its permissions. Every blaze crate moves onto
it; later it is the `std` of the blaze edition, where the sandbox of section 8 is derived from
what `main` hands out. Spec: [std](plan/design/std.md).

### 14. Its own edition

`edition = "blaze-2026.11"` is Rust with ten opt-in changes: handles that copy themselves, error
unions, view types, records and named arguments, contexts (`with`), effects (panic, alloc, block,
diverge) declared at the border, values that must be used up, contracts with pattern types, compile-time reflection, and `&uniq` as the spelling of the unique reference.
It removes lossy `as`, `async`, ambient IO, the global allocator and `static mut`. The lint group
`blaze::edition` gets a crate ready before the edition exists. Sections 12 and 13 are its parts.
Spec: [edition](plan/design/edition.md).

## How the work is run

The old way (70 agent windows, lanes that owned paths, request files between them, nothing
compiled until every lane said done) produced code nobody had run and notes nobody could read.
This replaces it.

**Decisions are rows.** Each decision is a row of [decisions.json](plan/decisions.json): what was
decided, what was recommended, why, and where it stands. A later row that reverses an earlier one
says so.

**Every agent has one task.** A task is a named piece of a decision or of a design's unbuilt part,
with its outcome and the check that decides it, each in one line, the paths it may touch and what
"done" prints. An agent with no task is not started. The agent app's sidebar lists tasks with their
agents under them ([tasks](plan/design/tasks.md)).

**The loop**, run by one coordinator (the main agent of a session):

1. **Measure** on main's exact tree: run the task's check before starting, never trust a
   remembered status. A task with no check gets one written first.
2. **Brief** each task with its decision or spec, the paths it may touch and what "done" prints.
3. **Write.** Workers only write and commit, all in one directory (no worktree per lane), and
   build and run nothing; the coordinator does small pieces itself.
4. **Build once.** The coordinator builds the one tree once, on the Mac, and admits what passes:
   one `blaze check`, one `blaze test`, then each task's check. What fails goes back to its worker
   once with the output; a second failure is recorded as what blocks it, and the task stops.
5. **Land** what passes through a pull request (`land.py`), and record the decision as a row.

**Docs stay small.** This page, the decisions, the bounds and one spec per unbuilt design. No lane
notes, no pass reports, no indexes of indexes: what was done is the pull request and the commit; a
measurement worth keeping is a `record` under `fed1eb428d:docs/reports/`. A design that changes edits its
spec in place. When a section here stops being true, fix it in the same change.

## Settled

Decided; object with evidence, don't work around.

- No daemon: a command answers and exits; every checkout on the machine shares its records. An
  optional helper (the file-event journal, with an expiry) may live between commands; nothing
  depends on it for correctness.
- No IDE and no MCP. The agent API is the interface.
- No GPU in the compiler; SIMD and huge pages are chosen at run time and every path is
  bit-identical.
- No kernel floor: each operation uses the best interface the running kernel has, chosen once at
  start from probed `KernelFacts`; where none fits blaze stops and says why.
- No fibers and no async inside the scheduler: a task that lacks a fact returns `Need`; no IO
  inside a job.
- The front end's algorithms may be redone wherever a faster design exists; the bar is behaviour,
  proved by real projects' tests and the oracle.
- `unsafe` only where it buys measured speed, behind a safe API, with `// SAFETY:`.
- Finished code only: no `todo!`, no `unimplemented!`, no fallback.
