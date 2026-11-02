---
status: plan
summary: blaze v2: the plan, as an index. Start here; each line links to the detail.
date: 2026-10-29
---
# blaze v2

blaze remade for speed: one warm VM per project where every branch, PR and CI job is a view;
results keyed by content and invalidated by the facts they read; per-function work on every
core; one zero-copy store; CI on the same VM. It lands as one pull request from `blaze-v2`
when everything passes with v2 as the only path.

**Everything is ours, at every level, for the best performance possible.** That covers the
crates, the interfaces, the data structures, the vendored dependencies (salsa, cranelift,
LLVM's bindings, the trait solver, std's build), the toolchain, the kernel interfaces, the file
formats, the CI and the machines. If changing something makes blaze faster and keeps Rust's
semantics, change it. When it isn't in your lane's paths, change it through a request
(`v2/requests/<lane>.md`). The full rule is in [plan/goals/principles.md](plan/goals/principles.md).

## How the plan is laid out

```
plan.md                  this page: what v2 is, where everything is
plan/
  goals/                 why and how much
    principles.md        the rules: everything is ours, deterministic, finished code, what agents run
    targets.md           the numbers v2 must hit
    risks.md             what can go wrong, what is left out
  design/                what v2 is
    model.md             one warm VM, branches as views
    data.md              data-oriented layout, compression tiers
    hardware.md          SIMD, huge pages, io_uring, no GPU
    prior-art.md         Flecs, ECS and game-engine techniques and where each lands
    memory.md            game-engine memory: allocators by lifetime, reserved regions, pools, handles, budgets
    concurrency.md       reads never lock, immutable values, task-boundary reclamation, no build lock
    frontend.md          rust-analyzer's design choices that cost a compiler, and v2's fixes
    restore.md           instant restore: mapped image, stable IDs, OS change journals
    compile-time.md      lazy per-function compiles, copy-and-patch, no binary on disk, global cache, semantic edits
    unsafe.md            unsafe only where it pays, behind safe APIs, proved by our own Miri, sanitizers, model checking, fuzzing
  lanes/                 who builds which part
    <lane>.md            a lane's plan, issues it takes in, and its Done when list
    <lane>/              that lane's own design notes (core/plane.md, cargo/notes.md, ...)
  process/               how it gets built
    building.md          lanes, the freeze, checkpoints, the first compile, integration
    decisions.md         settled; object in a request, don't reopen
    status.md            where each lane is, latest first
  reference/             look up as needed
    crates.md            which crate holds which types, who owns which paths
    glossary.md          the words
```

## Read in this order

1. **[principles](plan/goals/principles.md)**, **[targets](plan/goals/targets.md)**,
   **[model](plan/design/model.md)**.
2. **Your lane's file** (below), then **[crates](plan/reference/crates.md)**.
3. As your work needs them: the rest of `design/`, [building](plan/process/building.md),
   [decisions](plan/process/decisions.md), [glossary](plan/reference/glossary.md),
   [status](plan/process/status.md), other lanes' files.

## Lanes

| lane | what |
|---|---|
| [core](plan/lanes/core.md) | sans-IO core, drivers (io_uring, kqueue, IOCP, simulation), scheduler, memory admission |
| [vfs](plan/lanes/vfs.md) | Merkle VFS, views, file watching |
| [format](plan/lanes/format.md) | IDs, zero-copy columns, flat syntax, HIR layout, span tables |
| [items](plan/lanes/items.md) | item hashes and facts: minimal invalidation |
| [db](plan/lanes/db.md) | our own query database, types, trait solver, borrow check |
| [store](plan/lanes/store.md) | the content-addressed store and its tiers |
| [codegen](plan/lanes/codegen.md) | per-function codegen, instances, MIR; the lane's design: [v2-codegen.md](../v2-codegen.md) |
| [link](plan/lanes/link.md) | parallel in-place linker, live patching |
| [usercode](plan/lanes/usercode.md) | build scripts, proc macros, tests, sandboxes |
| [ci](plan/lanes/ci.md) | the agent API and the VM as CI |
| [proof](plan/lanes/proof.md) | the oracle, the rustc differential corpus, budgets |
| [cargo](plan/lanes/cargo.md) | workspace load, unit graph, CLI, std |
| [diagnostics](plan/lanes/diagnostics.md) | rustc's errors and warnings (same codes, lints, spans; our own text) |

## Where the old section numbers went

| section | now |
|---|---|
| 1 targets and principles | plan/goals/targets.md, plan/goals/principles.md |
| 2 machine model | plan/design/model.md |
| 3-11 | the lane files (3, 4 core; 5 format; 6 items, 6.5 usercode; 7 db; 8 store; 9 codegen and link; 10 ci; 11 proof) |
| 12, 18, 19 lanes, added lanes, integration | plan/process/building.md |
| 13 issues | each lane file's "Issues this lane takes in" |
| 14 risks, left out | plan/goals/risks.md |
| 15 what agents may run | plan/goals/principles.md |
| 16 hardware | plan/design/hardware.md |
| 17 data-oriented | plan/design/data.md |
