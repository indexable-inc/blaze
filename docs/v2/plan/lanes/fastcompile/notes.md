---
status: plan
summary: blaze v2 lane fastcompile: which crate builds which of the twelve changes of plan/design/compile-time.md, who owns it, and the boundaries between them.
date: 2026-10-29
---
# fastcompile: crates and boundaries

The plan is [compile-time.md](../../design/compile-time.md). Each idea's code is in a crate
of this lane's; hooks in other lanes' crates go through their request files. Every child keeps
its own notes beside this file (`<part>.md`) naming the state of the art it matches or beats.

| idea | crate or path | child |
|---|---|---|
| 1 lazy compilation | `crates/blaze-lazy/` | `fastcompile-lazy` |
| 2 shared generic code | `crates/hir-ty-share/` | `fastcompile-ident-share` |
| 3 exact test selection | `crates/blaze-testsel/` (executed, not reached, functions; the cached mark), hooks in usercode, ci, cargo | `fastcompile-ident-select` |
| 4 copy-and-patch codegen | `crates/blaze-stencil/` | `fastcompile-stencil` |
| 5 hash-consing | `crates/blaze-hashcons/` | `fastcompile-ident-hashcons` |
| 6 no binary on disk | `crates/blaze-inproc/` | `fastcompile-inproc` |
| 7 premapped dependencies | `crates/blaze-premap/` | `fastcompile-premap` |
| 8 one global cache | `crates/blaze-global/` | `fastcompile-global` |
| 9, 10 semantic edits, declared intent | `crates/blaze-intent/` | `fastcompile-intent` |
| 11 scheduling from history | `crates/blaze-history/` | `fastcompile-sched` |
| 12 PGO and BOLT | `tools/pgo/` | `fastcompile-sched` |

## Dependencies between this lane's crates

No cycles: a crate depends only on those above it in this list.

1. `blaze-history`, `blaze-hashcons`, `hir-ty-share`, `blaze-global` (none of this lane's).
2. `blaze-inproc`: the dual-mapped code memory and the process with no executable on disk.
3. `blaze-stencil`: a codegen backend with its own tier-up counters; depends on none of this
   lane's.
4. `blaze-premap`: maps its image through `blaze-inproc`.
5. `blaze-lazy`: stubs placed and patched through `blaze-inproc`; compiles with
   `blaze-stencil` in dev and test runs.
6. `blaze-intent`: drives the others through the agent API.

## The boundary of `blaze-inproc` (written first; `blaze-lazy` and `blaze-premap` use it)

`blaze-inproc` owns the memory and the process; it knows nothing of stubs, functions or
dependency graphs:

- **`CodeMemory`**: one OS memory object (Linux `memfd_create`, macOS a Mach memory entry,
  Windows a pagefile-backed section) mapped twice: writable in the daemon, executable in the
  run process. Reserve, write into the daemon's view, publish a range with the platform's
  instruction-cache maintenance. No `mprotect` after setup and no link.
- **The run process** (decided 2026-10-29, replacing a loader of blaze's own, under which a
  program saw the loader as its executable and broke superset-of-rust): the image's bytes
  are written once per layout to the path cargo gives the executable, keyed by content and
  never rewritten for a patch; the kernel execs that file, so `current_exe`, `/proc/self/exe`,
  dyld's and the PEB's names and backtraces see the program; before `main`, the linked-in
  runtime overlays the code ranges with the shared mapping, byte-equal at the swap. "No binary
  on disk" is "no link and no rewrite" ([inproc.md](inproc.md)).
- **Fences**: a word changed in code a thread may be running (a slot, a stub's target) is one
  aligned atomic store; code that is replaced stays mapped until every thread of the process
  has passed a quiescent point after the store (epochs per thread, read at the points
  `blaze-live` already polls at, and at task boundaries in the daemon's view of the process).
  Freed ranges are reused only after that.
- **The channel** between the run process and the daemon: the start handshake as fixed-layout
  little-endian frames with bounds checks, then asks (the code of function `n`) through
  `blaze_inproc::ask`: lock-free slots in shared memory, safe from signal handlers.

`blaze-lazy` asks `blaze-inproc` for: code memory ranges, an atomic publish of a slot word,
retirement of replaced code behind a fence, the ask slots, and the counter cells tier-up
reads (`CodeMemory::cell_view`). `blaze-premap` asks it for: the dependency image mapped at a
linked address inside the executable's reservation.

## Rules every child keeps (from the plan)

- Generic instances share code only where Rust can't observe the difference (idea 2).
- Copy-and-patch output behaves the same as cranelift's on the corpus (idea 4).
- Patching through dual mappings is sound for running threads (ideas 1, 6).
- Global-cache values are verified by content hash (idea 8).
- Lazy compilation is for dev and test runs; release builds and `blaze build` outputs stay
  whole (idea 1): a release `Settings` asked to compile lazily is an error.
- Semantic edits and declared intent are additions to the agent API; text edits keep working
  (ideas 9, 10).
