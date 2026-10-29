# blaze's own tree on a fresh machine with a warm remote (2026-10-29)

Lane warmstate. A file named by an `include!` in a function body is only seen once that body
has been compiled, so a cold build loads it and builds again. Before
[#1479](https://github.com/indexable-inc/blaze/pull/1479), that second build touched every
crate, and on a fresh machine whose patch base came from the store, `Base::stale` then compiled
the whole program again to compare marks. On blaze's tree the includer is `rustversion` (its
build script's `version.expr`, included in a body), so every fresh-machine build of blaze paid
for this.

## Method

GitHub `ubuntu-24.04` runners (4 vCPU, 16 GB plus 20 GB of swap). Workflow
`warmstate-probe.yaml` on branch `wsprobe-fresh` (never merged), run 40798682842, attempt 1. Two
jobs ran in parallel: *base* is main at `6a02fbfdc1`, *fix* is #1479's first commit on it. Each
job cargo-builds `blaze-engine --release --features llvm` and its proc-macro server, then runs
`harness/freshwarm.py blaze --edits none,top,body` on a second checkout of the same commit. In
that run, a seed builds `-p blaze-engine --release` and pushes to a directory remote. A fresh
home and store at the same paths then pull from the remote and build, once with no edit, once
with a body edit in `blaze-engine`'s `content.rs` (top) and once with a body edit in `hir-def`'s
`is_block_module` (body). `base.json` and `fix.json` are freshwarm's reports.

## Numbers

| fresh machine | compiled, base | compiled, fix | late-include rebuild, base | fix | wall, base | fix |
| --- | --- | --- | --- | --- | --- | --- |
| nothing changed | 355,042 | 812 | 94.0 s | 3.5 s | 451 s | 480 s |
| body edit, top crate | 376,421 | 22,213 | 91.4 s | 2.3 s | 461 s | 589 s |
| body edit, hir-def | 264,471 | 146,047 | 48.6 s | 3.3 s | 481 s | 702 s |

The wall times can't be compared directly. The fix job's runner was about 1.7 times slower on
every phase the change doesn't touch, with identical work: seed 1370 s against 2214 s, load
72.9 s against 126.1 s, build scripts 46 s against 77 s, and 20,292 against 20,309 LLVM
functions in the second program. Measured by itself, the phase the change touches dropped by
45 to 89 s in every case.

## What is left

On the fix's runner, with no edit, 480 s are spent mostly outside compilation:

- 126 s loading the workspace: build scripts 77 s and the files 48 s. This is the
  coldfast/sansio lanes' area.
- 24 s of wasm proc macros, of which only 1 of 2,887 calls came from the store ("43 not kept,
  as they asked the database").
- 17 s resolving.
- About 40 s computing roots and entries from the store.
- The whole-source check of each program.
- Reading the store for the second and third programs (`blaze_engine`, `blaze_rustc`), which
  are built one after another in the same worker.

The edit cases still compile every crate that depends on the edited one, because an edit
renames those crates (`content.rs`). That is #344's code keys, on `blaze-next`.
