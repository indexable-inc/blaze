---
status: plan
summary: lane mem's child mem-codegen: moving codegen units, MIR bodies, stencil emits, the fastcompile crates and usercode's warm paths (the check of a kept run, build-script elision, the rerun check) onto crates/blaze-mem's frames and scratch, with the allocation-budget tests that hold each warm path to zero.
date: 2026-10-29
---
# mem-codegen: codegen, MIR, stencils and usercode onto blaze-mem

Parent: lane mem (docs/v2/plan/design/memory.md). None of these crates is free (lanes codegen,
fastcompile and usercode are still working), so every change is a request, written with its
code before and after and the budget test that holds it:

| crates | requests | written by |
|---|---|---|
| `hir-ty-codegen`, `blaze-codegen` | `v2/requests/mem-codegen-unit.md` | `mem-codegen-unit` |
| `hir-ty-mir` | `v2/requests/mem-codegen-mir.md` | `mem-codegen-mir` |
| `blaze-engine` | `v2/requests/mem-codegen-engine.md` | `mem-codegen-engine` |
| `blaze-stencil`, `blaze-premap`, `blaze-global`, `blaze-intent`, `blaze-history` | `v2/requests/mem-codegen-stencil.md` | `mem-codegen-stencil` |
| `blaze-usercode`, `blaze-build-script` (and the `Inputs` implementations in `blaze-expand`, `blaze-macro-wasm`, `blaze-testsel`) | `v2/requests/mem-codegen.md` | `mem-codegen` |

`blaze-lazy` and `blaze-tier` are mem-backend's (docs/v2/plan/lanes/mem/backend.md).

## The rule each crate follows

- **One unit of work, one frame.** A codegen unit (one function lowered and emitted), a MIR body
  built and run through its passes, a stencil emit, a check of a kept run and a read of a build
  script's root file each run inside `blaze_mem::frame::task(tag, ..)`: `Tag::Codegen` for
  units and emits, `Tag::Mir` for bodies, `Tag::Usercode` for usercode. Their temporaries are
  `Vec<T, &Frame>` and frame slices, freed together when the task returns, with no free per
  value. A task inside a task (a listing hashed during a walk, a pass inside a query run) gets
  a frame of its own, so an inner reset never frees what the outer task holds.
- **What outlives the unit is written once, into its destination.** A `CompiledFunction` that is
  kept, the code an emit keeps and a build script's printed output are not frame values. They
  are built in a buffer the caller reuses (a `scratch::lend_bytes`, a `Scratch<T>` in worker
  state, or the caller's own `&mut Vec<u8>`), then copied once into the store or the result.
  `elide::elide` shows the shape: it prints into the caller's `out`, and every other byte it
  touches is the frame's or a lent string.
- **Per-call scratch is the thread's.** Path joins (`scratch::with_path`), formatting
  (`with_string`) and sort orders (`with_u64s`) are lent from the thread's shelf. A callee takes
  them itself instead of asking its caller for a buffer, as `Rerun::stale` used to.
- **Values on a hit are lent.** A check reads each input where it lies (`Inputs::env` lends
  `&OsStr`). A directory listing is packed in the frame (`trace::Names`: one byte buffer and
  spans on Unix), not built as a `Vec<(OsString, bool)>` of fresh strings.

## Budget tests

Each warm path has a `blaze-alloc-count` test that measures the second run on one thread:

| path | test | budget |
|---|---|---|
| a refused build script | `blaze-build-script` `elide/tests.rs` `what_elision_allocates` | 0 |
| a read build script, warm | the same | 0 (cold: 2, the caller's buffer and a lent string) |
| a fresh rerun check | `apply/tests.rs` `a_fresh_run_is_checked_without_allocating` | 0, with no caller buffer |
| a check of a kept run's shape | `blaze-usercode` `index.rs` `a_warm_check_of_a_shape_allocates_nothing` | 0 (Unix) |
| a package walk and a listing | `trace.rs` `a_warm_walk_allocates_nothing` | 0 (Unix) |
| a codegen unit's re-emit | `v2/requests/mem-codegen-unit.md` | 0 |
| a MIR pass over a body | `v2/requests/mem-codegen-mir.md` | 0 |
| a unit re-driven by the engine, a no-op rebuild's walk | `v2/requests/mem-codegen-engine.md` | 0 |
| a stencil emit | `v2/requests/mem-codegen-stencil.md` | 0 |

Off Unix a name has no safe packed form (`OsStr::from_encoded_bytes_unchecked` is unsafe, and
`blaze-usercode` forbids unsafe code), so `Names` keeps an `OsString` per name there. This is
the same split `trace::os_bytes` and `os_in` already make. The two usercode zero tests are
`#[cfg(unix)]` for that reason.

## Speed of light

- **A frame allocation** is an align, an add, a compare and two stores: about 2 ns
  (`crates/blaze-mem/src/frame.rs`). A global `malloc` and `free` pair costs about 20 to 50 ns
  and touches shared allocator state. A codegen unit of a few hundred values makes tens of
  temporaries, so frames save on the order of a microsecond per unit. More importantly, they
  remove the allocator from the critical path of every worker. Resetting a frame is one store,
  so the frame cost of a unit is its bump allocations alone.
- **A warm check of a kept run** is bounded by its reads (a vouched file is a table lookup, a
  view directory one query read) and one BLAKE3 pass over the shape's bytes plus 32 bytes per
  input. For a script naming 20 inputs that is about 1 µs. Before these requests, a script with
  10 variables and a package walk of 200 names made about 230 allocations (5 to 10 µs), 5 to
  10 times the bound. After them it makes none, so the check costs its reads and its hash.
- **Elision** of a script it accepts is one pass over the source (about 1 GB/s for the lexer,
  docs/v2/plan/lanes/usercode/speed-of-light.md) plus the bytes it prints. The three
  allocations it made per read cost more than lexing a 200-byte script; warm, it now makes none.

## Per crate group

- **Codegen units** (`hir-ty-codegen`, `blaze-codegen`): the backend opens one frame per unit
  in its worker job loops (`units.rs:365`, `mod.rs:6434`). `FunctionCx`'s maps and lists and
  the other per-unit tables live in it, through a new `frame_map.rs` (`FrameHashMap`,
  `FrameHashSet`, `FrameMap<Idx, V>`, with hashbrown's `nightly` feature). Only maps that are
  never iterated move into the frame, so output order is unchanged. Relocation and line rows
  are reused per worker beside the Cranelift contexts, which `spare.rs` already reuses with
  `ctx.clear()`. Kept outputs are written once at their exact size. Cranelift's IR, its
  `MachBuffer` and postcard's `to_allocvec` take only global `Vec`s, so they stay on the heap,
  each named.
- **MIR** (`hir-ty-mir`): one frame per lowering, per pass of `run_passes` and `optimize`, per
  `monomorphized` and per `frame_storage_conflicts`. The per-body `Ctx` gains its frame
  lifetime. The memoized body stays the query's owned result, copied once at its exact size
  (`MirLowerCtx::finish`). Warm passes are budgeted at 0.
- **The engine** (`blaze-engine`): no frame per unit. It gets per-build memo tables on
  `Column` plus `swiss::Index`, so a warm unit's questions to the resolver are borrowed
  answers. Per-build lists go in a request `Arena`, and the no-op rebuild's walk and the
  unwatched-file query use lent buffers.
- **Stencils** (`blaze-stencil`): each compile's buffer, plan and side tables are frame values
  (`Buf<'f>`, `Plan<'f>`, `Side<'f>`). The output is lent as a `CodeRef<'_>` and copied once
  into a `Code` the caller reuses, so a warm compile into a reused `Code` allocates nothing.
  The other fastcompile crates move their request temporaries onto frames: `blaze-global`
  (`Tag::Store`), `blaze-intent` (`Tag::Api`) and `blaze-premap` (`Tag::Link`).
  `blaze-history` already allocated nothing.
- **Usercode**: the check of a kept run, elision and the rerun check, described above.

## State

Every request is written (`v2/requests/mem-codegen.md` and its four children's files), and each
owner has been told. Nothing is applied yet: the owners apply the requests, or the coordinator
does once the owners' lanes are done.
