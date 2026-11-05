---
status: plan
summary: blaze v2: how it is built: lanes, integration, the freeze.
date: 2026-10-29
---
# How v2 is built

## 12. How it is built: lanes on one integration branch

Each lane is a cursor-agent (`--yolo`, Opus 5.5 medium) in its own worktree, on a branch off
`blaze-v2`, merging into `blaze-v2` with `--no-ff` when its code builds. The coordinator fixes
the interfaces first (step 0 below) and owns them afterwards: a lane that needs an interface
changed asks the coordinator, who changes it on `blaze-v2` for every lane at once.

**Main is frozen** for the crates v2 rewrites while v2 is built: bug fixes only, each one
merged into `blaze-v2` by the coordinator the same day. Other crates keep landing on main as
usual and are merged in daily.

| lane | owns | depends on |
|---|---|---|
| `v2` (coordinator) | the profile of today, the interfaces, the branch, main merges, the full suite, the final PR | — |
| `v2-core` | sans-IO core, drivers (io_uring, kqueue, simulation), scheduler, admission | interfaces |
| `v2-vfs` | Merkle VFS, views, journal, file watching | core |
| `v2-format` | the zero-copy format and schemas, both kinds of IDs, sharded interners, flat syntax (replacing rowan), SoA HIR, span side tables | interfaces |
| `v2-db` | our own query database from `vendor/salsa`: input-hash keys, fingerprints, views, signature/body split, GC, daemon image, parallel expansion, the rerun explainer | format |
| `v2-items` | item hashes (interface, layout, code, deep), facts, negative-fact indexes, rename and move | db |
| `v2-store` | packs, index, compression, chunking, verification, remote tier | format, core |
| `v2-codegen` | per-function units, instance keys, code dedupe, release inlining edges | items, store |
| `v2-link` | the parallel in-place linker and commit, debug side table, live patching | codegen |
| `v2-usercode` | traced-input sandboxes for build scripts and proc macros, wasm proc macros, warm process pool, test selection | core, store |
| `v2-ci` | the VM as CI: webhook, views per PR, check runs, the train | vfs, db |
| `v2-proof` | the invalidation oracle and fuzzer, the crate corpus differential, performance budgets | all, from day one against v1 |

Order:

0. **Interfaces** (coordinator, first): the effect and completion types, the task and admission
   API, the ID and arena types, the query key and fingerprint types, the item-hash and fact
   types, the store's get/put API. Written as crates with types and doc comments and no logic,
   merged into `blaze-v2` before any lane starts.
1. All lanes in parallel against the interfaces, writing code and its tests, compiling and
   running nothing (section 19) until every lane is done.
2. `v2-proof` builds the oracle, the fuzzer, the corpus and the budgets in parallel, and sets
   the bar by running them against v1.
3. Integration, all at once: everything is run together on `blaze-v2`; failures go back to
   the owning lane; v1's paths are deleted and the IDE crates v2 doesn't need are removed; the
   branch is green on the whole gate with no v1 path left.
4. One PR to main through `land.py`; the docs this supersedes (`driver.md`, `INCREMENTAL.md`'s
   plan parts, `remote-cache.md`, `linker.md`) marked or rewritten in the same PR.



## 18. Lanes added after the first plan

- **`cargo`**: everything cargo-shaped that no lane had: the workspace load (what `load-cargo`
  does today, the 238 s of #1450's smoke job) rebuilt as queries over a view of `Cargo.toml`s
  and lock files; the unit graph as queries; `blaze-cargo`'s daemon and client moved onto the
  plane, `blaze-core` and `blaze-api`; the CLI (`blaze build`, `run`, `test`, `check`, and
  `cargo blaze`) as a client of the agent API with cargo's flags, output and exit codes
  unchanged (superset-of-rust: users see cargo's behavior); the standard library as one more
  crate graph built through the same pipeline (no-rustc F, #86), its results in the shared base
  layer. Owns `crates/blaze-cargo/` but store's and usercode's paths, `crates/load-cargo/`,
  `crates/project-model/`, `crates/blaze-units/`, `crates/blaze-tree/`, `crates/blaze-manifest/`,
  `crates/blaze-metadata/`, `crates/blaze-resolve/`, `crates/blaze-registry/`,
  `crates/blaze-source/`, `library/`.
- **`diagnostics`**: compile errors and warnings judged by usefulness, not by matching rustc's
  codes, lints, spans or text (AGENTS.md `useful-not-parity`), computed in the compiler from the queries that found
  them, with the facts they read (for the agent API). What builds need from `ide-diagnostics`
  moves here; the IDE crates are then removed. Owns `crates/blaze-diagnostics/` (new) and
  `crates/ide-diagnostics/` until it is removed.

- **`safety`**: proves the unsafe code sound without rustc (`plan/design/unsafe.md`): the
  undefined-behavior checker on the MIR evaluator (`crates/hir-ty-ub/`, new), fuzz targets for
  every parser of untrusted bytes (`fuzz/`, new), and the model checker for lock-free code
  (`crates/blaze-check/`, new). Sanitizer instrumentation goes to codegen by request.
- **`fastcompile`**: the twelve changes in `plan/design/compile-time.md`, in new crates
  (`blaze-stencil`, `blaze-lazy`, `blaze-premap`, `blaze-intent`, `blaze-history`,
  `blaze-global`) and `tools/pgo/`, with hooks in other lanes' crates by request.
- **`frontend`**: makes `plan/design/frontend.md` (rust-analyzer's design, fixed) and
  `plan/design/restore.md` (instant restore) true across lanes, building what no lane started.
- **`mem`**: `plan/design/memory.md`, game-engine memory management: builds `crates/blaze-mem` and
  moves the hot crates onto it.

## 19. How the coordinator integrates

- **Nothing compiles until every lane is in.** No build, check or test runs, by agents or by
  the coordinator, until every lane reports `STATUS: DONE`. Checking half-written crates only
  finds that they are half-written. Until then the coordinator applies requests, commits and
  merges main, and the only check anyone runs is the manifest lint, which builds nothing.
- **Checkpoints**: every hour the coordinator commits the whole worktree and runs the manifest
  lint (`python3 tools/v2/manifest_lint.py '{"root": "."}'`, under a second). It finds
  `workspace = true` dependencies the root doesn't declare, path dependencies with no
  `Cargo.toml`, and members with no target file. Each problem goes to the lane that owns the
  path. Any agent may run the lint too.
- **A new crate starts loadable**: its `Cargo.toml` and a `src/lib.rs` that declares its
  modules arrive together, and each dependency it adds to the workspace goes through a request
  first. One unloadable member stops cargo from loading the whole workspace.
- **The first compile** runs once every lane is done, in one ix VM sized for it (32 cores and
  128 GB committed, `--memory-commit`). Its job count is bounded by that memory, so it never
  OOMs. It checks each lane's crates on their own (`blaze check -p` per lane, in dependency
  order, continuing past failures), so one broken crate can't hide every other lane's errors.
  Each lane gets back its own error list, and the lanes fix their errors in parallel. Only
  then does the whole gate run.
- **Main merges** happen only at a checkpoint, right after the commit, so a merge never meets
  an agent's uncommitted edit. A conflict in a lane's path goes to that lane.
- **The freeze** is a pinned issue on main naming the crates v2 replaces; on them, main takes
  bug fixes only, and the coordinator merges each into `blaze-v2` at the next checkpoint.
- **Sections**: the lane files keep the plan's first numbering (6.2 is items, 6.5 usercode,
  9 codegen and link). The rest map through the last table of [plan.md](../../plan.md).
