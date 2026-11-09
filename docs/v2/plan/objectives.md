---
status: plan
summary: The ledger. Every objective of the plan, the check that decides it and where it stands; work is picked from here and nowhere else.
date: 2026-11-09
---
# Objectives

One row per outcome. The **check** is a command that passes or fails on a named tree; a row whose
check says `write:` has none yet, and writing it is the first part of the work. How rows are
picked, worked and landed is [How the work is run](../plan.md#how-the-work-is-run).

**Status** is `met <commit>` (the check passed on that commit of main and is in the gate or the
train), `open` (the check ran and failed; one line says by how much or on what), `blocked: <what>`
or `?` (not measured since this ledger was written). Every row starts at `?`: the lane notes this
replaces claimed "done" for code nobody had run. The first job is to run each check on main and
write down what it says. Figures in "last known" are from 2026-10-29 and are leads, not status.

A budget is a name `harness/v2/budgets.py` takes in `budgets` (its docstring has the input); the
other harnesses are in [harness.md](../../harness.md).

## Targets

What a user feels, on one warm machine (Graviton 5 or EPYC Turin; an M-series Mac for macOS).

| id | objective | check | last known | status |
|---|---|---|---|---|
| T1 | A body edit in nushell is running code in 10-30 ms | budget `body` | 0.29 s | ? |
| T2 | A signature edit costs in proportion to the items whose facts changed | budget `signature` | 0.31 s, 6,269 roots asked | ? |
| T3 | A format or comment edit compiles nothing | budgets `format`, `comment` | | ? |
| T4 | A rename or a move of an item compiles nothing | budgets `rename`, `move` | | ? |
| T5 | A no-op build of ripgrep takes under 5 ms | budget `noop` | 11 ms | ? |
| T6 | A warm daemon start takes under 1 s | budgets `warm-start`, `restart` | 35 s nushell, 238 s in CI | ? |
| T7 | blaze's own CI on a small pull request takes under 2 min | budget `ci` | 30 min+ | ? |
| T8 | A cold build of blaze keeps every core busy; wall time is within 10% of its critical path | budget `cold-blaze` | 12 min, mostly serial codegen | ? |
| T9 | No build is killed for memory; peak is at most cargo's | budget `memory`, `harness/v2/tightmemory.py` | ripgrep 2.0 GB vs 0.56 GB | ? |
| T10 | A body edit in a big top crate (ruff, ty) takes under 50 ms | budget `big-crate` | 75 s, 147 s | ? |
| T11 | Dev-build code runs at least as fast as cargo's debug build on ripgrep | budget `dev-speed` | 5.8x slower | ? |
| T12 | Release code runs at least as fast as cargo's release build | budget `release-speed` | | ? |
| T13 | A second build of an unchanged tree in a fresh checkout misses nothing | budget `cache-misses`, `harness/v2/worktree_keys.py` | | ? |
| T14 | Every bound in [bounds.md](bounds.md) is within 10% | budget `bounds` | | ? |

## Correctness

| id | objective | check | status |
|---|---|---|---|
| C1 | The real projects of the corpus build with no flags and pass their own test suites | `harness/v2/realprojects.py`, `harness/v2/corpus.py` | ? |
| C2 | Every incremental build equals a build from nothing, with no stale result and no needless rerun | `harness/v2/oracle.py` | ? |
| C3 | Two builds of one tree give the same bytes, on any core count | `harness/v2/determinism.py` | ? |
| C4 | blaze builds, checks and tests blaze; no step of the gate, the train or a release runs cargo or rustc | write: a gate step that fails when a rustc or cargo process is spawned | ? |
| C5 | blaze runs rustc nowhere ([no-rustc](../../no-rustc.md) lists what is left) | the steps of no-rustc.md, each with its own check | ? |
| C6 | Every program rustc rejects, blaze rejects with the same code and primary span, and every program it accepts, blaze accepts | rustc's UI tests through `harness/v2/corpus.py` (rejections) | ? |
| C7 | No `todo!`, `unimplemented!` or fallback path in `crates/` | write: a lint in `blaze check` | ? |

## The design, part by part

Each row is the part of [the design](../plan.md#the-design) not yet shown by a check. Section
numbers are the plan's.

| id | § | objective | check | status |
|---|---|---|---|---|
| D1 | 1 | Commits, rebase, diff, journal recovery and collection work, with modes and symlinks, through torn writes and power cuts | the vfs simulation tests in `blaze test` | ? |
| D2 | 1 | A change on disk becomes a transaction on Linux (fanotify), macOS (FSEvents) and Windows (USN), and a restart learns what changed while down without reading file contents | write: a test per platform that edits while the daemon is down | ? |
| D3 | 2 | Every item has interface, layout, code and deep hashes; names and positions enter a key only where a program can observe them | budgets `rename`, `move`, `comment`; `harness/v2/incraudit.py` | ? |
| D4 | 2 | No key holds an absolute path | `harness/v2/worktree_keys.py`, `harness/v2/share.py` | ? |
| D5 | 3 | hir-def, hir-expand and hir-ty run on blaze-query; salsa is in no build | write: `blaze check` ban on `salsa` paths | ? |
| D6 | 3 | Name resolution, impl tables and trait candidates are maintained by deltas; a one-item edit touches work in proportion to the delta | `harness/v2/dbsp`, budget `signature` | ? |
| D7 | 3 | `blaze why` names the read or key part behind every rerun of the last build | write: a test that edits one input and expects exactly that reason | ? |
| D8 | 4 | Flat syntax has replaced rowan in every build; HIR, MIR and types are columns within their size asserts | write: `blaze check` ban on `rowan` paths | ? |
| D9 | 4 | A restart maps the image and decodes nothing; a restored daemon equals a fresh one byte for byte | budget `restart`; the restore round-trip test | ? |
| D10 | 5 | The store's tiers, index flip and collection survive crashes and failed syncs | `blaze-storemodel` (the model checker) | ? |
| D11 | 5 | The remote cache is the local store's keys off the critical path; a cold build of a cached tree compiles nothing | budget `cache-misses` against a remote | ? |
| D12 | 5 | A sampled share of store hits is recomputed and compared in every train run | `BLAZE_VERIFY_STORE` in the train | ? |
| D13 | 6 | Every driver (io_uring, kqueue, IOCP, simulation) implements every effect; no thread blocks in the kernel outside the deadline wait | simulation tests over seeds; the `deadline-waits` lint | ? |
| D14 | 6 | Work is admitted by measured peak memory under one budget; one evictor serves every cache | budget `memory`, `harness/v2/tightmemory.py` | ? |
| D15 | 6 | A cold build's order follows the critical path learned from history | budget `cold-blaze` | ? |
| D16 | 7 | Dev code is compiled lazily, per function, into the running process, with no binary on disk and no link | budget `body` | ? |
| D17 | 7 | A hot dev function is recompiled by the optimizing tier in the background and patched in | budget `dev-speed` | ? |
| D18 | 7 | An edit patches running code and rolls back when a layout or ABI check fails | the live-patch tests; write: a planted layout change the check must refuse | ? |
| D19 | 7 | blaze's linker writes ELF, Mach-O and COFF, in parallel, with debug info and panic locations | C1 on Linux, macOS and Windows | ? |
| D20 | 7 | Equal instances are one (polymorphization, the share key, folding) | `harness/v2/share.py` | ? |
| D21 | 8 | Build scripts and proc macros run sandboxed on Linux, macOS and Windows, keyed by their traced reads | C1 with the sandbox on; write: a test that reads outside and is refused, per platform | ? |
| D22 | 8 | A build script proved pure is keyed without running | `harness/v2/pure_scripts.py` | ? |
| D23 | 8 | `blaze test` runs only the tests whose reached code changed, one process each from a fork server | `harness/v2/reachable.py`; write: an oracle that every skipped test would have passed | ? |
| D24 | 8 | Doctests are built by blaze's front end, never rustdoc | C1 with doctests on | ? |
| D25 | 9 | Every request of the agent API is answered in one versioned CBOR schema; the CLI holds no logic the API lacks | write: a test that drives each CLI command through the socket alone | ? |
| D26 | 9 | Everything blaze prints is a `Report` first | write: `blaze check` ban on printing outside the renderer | ? |
| D27 | 9 | CI runs on the warm daemon: a webhook makes a view per pull request and reports a check run | budget `ci` | ? |
| D28 | 10 | `tracing` is in no build; every wait and every stretch of worker time is a span | write: `blaze check` ban on `tracing` paths; the unnamed-time check of `blaze explain` | ? |
| D29 | 10 | The live view, `blaze explain` and the explorer read one stream | write: one fixture stream rendered by all three | ? |
| D30 | 11 | Every lock-free structure has a model-checker test; every durable path a simulation test with a planted bug | write: a lint that lists atomics and renames with no such test | ? |
| D31 | 12 | Each extension is checked on one shared MIR walk and used in blaze's own crates | the extension's tests; its attribute present in `crates/` | ? |
| D32 | - | One install command gives a working blaze on Linux, macOS and Windows | write: an install test per platform in the train | ? |

## The agent app

The app in `crates/harness/` is where people watch the work. Its design is
[tasks](design/tasks.md).

| id | objective | check | status |
|---|---|---|---|
| A1 | Every agent belongs to a task; a sub-agent inherits its parent's | the daemon refuses a spawn with no task; model test | ? |
| A2 | The sidebar lists tasks, with their agents under them | offscreen shot test of the sidebar | ? |
| A3 | A task that waits on a person says so in the sidebar and in a system notification | model test of the `needs you` state | ? |
| A4 | A task row shows its current step, how long it has run, what it has cost, and its branch and pull request | offscreen shot test | ? |
| A5 | A finished task stays, with its outcome, until a person archives it | model test | ? |

## Open design questions

Decide with evidence, then fold the answer into the plan and delete the row.

| id | question |
|---|---|
| Q1 | Two verifiers are specified ([verus](design/verus.md), an SMT fork; [formal-verification](design/formal-verification.md), MIR to Lean 4). Keep both only if each proves something the other cannot; otherwise one. |
| Q2 | The explorer carries its own HTTP/3 and WebTransport stack ([http3-sans-io](design/http3-sans-io.md)). Show what it does that HTTP/1.1 streaming on a local socket cannot, or remove it. |
| Q3 | Several specs describe one concept and should be one each: scheduler, fibers and concurrency; memory, memory-budget and memory-pool; code-identity, item-hash, name-free-identity and caching; explore, explore-core and explore-generic; profile-history and perf-history. Merge when next touched. |

## Left out

Not planned: an IDE or language server, MCP, a GPU compiler path, building with any toolchain but
the pinned nightly.
