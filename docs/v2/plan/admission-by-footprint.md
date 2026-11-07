---
status: plan
summary: admission by footprint (lane admit-footprint-1106, 2026-11-06) - the plane admits against the process footprint plus grants not yet realized, not grants alone; freed-but-kept allocator memory is purged before work is refused or the daemon dies; a task that alone passes the ceiling fails with a typed error naming it; grants are graded against peaks; every task span names its kind, crate and unit; proved by a Hegel DST with planted bugs.
date: 2026-11-06
---
# Admission by footprint

Lane admit-footprint-1106, 2026-11-06.

## What broke

On a real build the daemon died at its memory ceiling (exit 77, `MEMORY_CEILING`) with 23.04 GiB
resident. Of that, the global heap held 8.36 GiB live and 15.88 GiB committed, so 7.52 GiB had been
freed but the allocator kept it. Grants were wrong by about 6x in both directions. Three codegen
tasks peaked at 10.5, 12.2 and 20.7 GB, and each was granted a flat 0.55 GB. Every `task` span had
NULL `function`, `crate` and `unit`, so the profile could not say which unit each one was.

The causes:

1. The plane judged room as `budget - max(granted, held)`, where `held` was `arena::held()`. That
   value counts live bytes only. It leaves out what the allocator keeps after a free, and it leaves
   out what a running task allocated beyond its grant. The real footprint could therefore be far
   above anything the plane saw.
2. Nothing gave kept memory back before the ceiling guard killed the daemon. The guard compared the
   footprint to the ceiling and exited.
3. When a key had no record, the grant was a per-kind estimate or a flat default (64 MiB unseen,
   0.55 GB after a few cheap observations). Nothing counted when the guess was wrong.
4. `run_measured` opened the `task` span with a kind and a node, but never with a crate or unit.

## The rules

- **Room is the footprint.** In footprint mode the plane charges
  `max(granted, footprint + Σ over running tasks of (grant - realized)⁺)`. `footprint` is the last
  reading of the process footprint. `realized` is what that task's memory account had reached at the
  reading. A task's grant is counted until the task's own bytes show up in the footprint, and is
  never counted twice. Headroom is `budget - charge`.
- **Purge before refusing.** When admission holds back work and the allocator keeps freed memory,
  the plane says a purge is due (`Plane::purge_due`). It asks once, and not again until a reading that
  follows a purge, and at most every `PURGE_EVERY_NS`. The driver keeps the request until it
  purges, then reads the footprint again and sends it with `purged`.
  - On macOS the purge is the heap give-back (`blaze_mem::tag::give_back_heap`), the chunk-cache
    trim, and `malloc_zone_pressure_relief(NULL, 0)` for the system zones.
  - On Linux the purge is the heap give-back, which is `madvise(MADV_DONTNEED)` on free spans, and
    the chunk trim.
- **The ceiling kills only what cannot fit.** The gate `footprint::past_ceiling` is pure and decides
  what to do when growth would pass the ceiling:
  1. `Take` the growth if it still fits.
  2. Otherwise `Purge` if memory is kept and no purge has been tried since the last reading.
  3. Otherwise `Fail` the asker with `TooBig` if it runs alone. `TooBig` names the task, its unit,
     its peak, what is held besides and the ceiling.
  4. Otherwise `GiveWay` to the newest waiting task when every other task is waiting.
  5. Otherwise `Wait`.

  The daemon's ceiling guard first asks for a purge (`CeilingLook::Purge`) and only then exits, if
  the footprint is still over the ceiling after the purge.
- **Grants are graded.** On `Done`, the plane compares each peak with the grant the task held:
  - `over_grant`: the peak passed the grant.
  - `under_grant`: the peak was under half the grant.

  The plane counts each case and its bytes in `Plane::grades()`, and the `task_memory` query shows
  both from the spans. Grants still come from blaze-history: first the key's record, then the kind's
  estimate, then a size-scaled estimate (`push_sized`).
- **Every task span names its subject.** `task` spans carry `function` (the task kind), `crate` and
  `unit`, interned on the task's own thread. The names come from `Work::subject`, which for a unit
  phase asks `Front::subject`.

## The sans-IO core

`crates/blaze-graph/src/footprint.rs` is `#![blaze::sans_io]`. It holds:

- `Charge`: the plane's view of memory. It has two modes:
  - grants mode, fed by `PlaneInput::Held`, which is the old accounting kept for `exec` and the old
    tests;
  - footprint mode, fed by `PlaneInput::Footprint` and `PlaneInput::Realized`.

  It also keeps the purge rate limit and the `Grades`.
- `past_ceiling`: the gate described in the rules above.

The plane gains two inputs:

- `PlaneInput::Footprint { at_ns, bytes, kept, purged }` is a reading of the footprint and of how
  much of it the allocator could give back. `purged` says the driver purged just before the
  reading. Only such a reading counts as following a purge: a reading taken after the plane asked,
  but before the driver purged, says nothing about what a purge gives back.
- `PlaneInput::Realized { at_ns, id, bytes }` is how much a running task's account has reached.
  When that passes the grant, the grant grows to it at once. The realized part leaves the
  unrealized sum only at the next `Footprint`, because only that reading contains it.

## Proof

`crates/blaze-graph/src/footprint/tests.rs` is a Hegel DST. It drives the real `Plane` and
`past_ceiling` against a simulated allocator. The allocator keeps freed memory until a purge, and
leaves a random part of it stuck for good. The test draws random peaks, some of them over their
grant, and a ceiling.

The oracle checks these:

1. The footprint never passes the ceiling, unless exactly one task runs and that task alone passes
   it. That task, and only that task, fails with `TooBig`.
2. `Wait`, `GiveWay` and `Fail` happen only when a purge could not help.
3. Every other task finishes within a step bound. There is no deadlock.

Two planted bugs are each caught by the oracle on a fixed seed list:

- `GrantsOnly`: admission by grants and live bytes alone, as before this lane.
- `NoPurge`: a gate that never purges.

## Left after this lane

- **No gate at allocation time.** Growth past a grant on the global heap is not gated where it
  happens. The daemon sees it at the next poll and charges it, so later work waits. The running task
  itself is not stopped. A gate inside the allocator's slow path, keyed by the task's account, is
  the next step.
- **No way to stop a running task.** The daemon has no means to make a running task yield:
  `PlaneOutput::Yield` is ignored. The DST's gate shows what that path should do.
- **Grants do not travel across profiles.** History keys include the profile, so a release build
  does not learn from the debug build of the same unit. A unit-level record that both profiles feed
  would remove the remaining under-grants on a first release build.
