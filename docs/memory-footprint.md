---
status: plan
summary: Why a stage5 self-build's daemon reached 75.7 GiB against a 47 GiB limit on the Mac (freed memory pinned by long-lived blocks on the same heap, query storage that only grows, two measures of memory), and the fix: an owner per lifetime (frames reset per item, outputs as records in the store's own vmcache-style buffer pool), footprint as the one measure, telemetry first.
date: 2026-11-09
---
# Memory: what the stage5 self-build showed, and the fixes

This plan applies [memory-budget.md](memory-budget.md) and [memory-pool.md](memory-pool.md) to
one measured build. It adds what that build showed and orders the next steps. Where it overlaps
those plans, they stay the design, and this note says which of their sections the step builds.

## The build

| | |
|---|---|
| What was built | blaze at `bee6a55292`, tree `d134260f0d13` (branch `stage5-1108`) |
| Built by | the 0.20261105.4 release, as stage 0 |
| Where | the Mac, `BLAZE_HOME=/tmp/rj-hn` |
| Limit | `BLAZE_DAEMON_MEMORY_GB=48`, a 47 GiB limit |
| Daemon | pid 51782 |
| Profile | build_id 54 |
| Result | built in 23m52s, after the footprint peaked at **75.7 GiB**. No exit 77 at the ceiling. The Mac swapped. |

### What the profile says

The counters table (`*.counters.parquet`: `footprint bytes`, `live bytes`, `frame bytes`, `pool
running`) is sampled many times a second. Per minute, for pid 51782:

| Minute | Footprint | Live heap | Footprint less live | Tasks running |
|---|---|---|---|---|
| 0 | 19.4 | 9.5 | 9.9 | 16 |
| 8 | 57.7 | 23.1 | 34.6 | 16 |
| 14 | 62.1 | 27.6 | 34.5 | 2 |
| 22 | **75.7** | 29.1 | **46.6** | 1 |

All values are GiB. The daemon's last breakdown in `serve.log` before it stopped agrees:

- the global heap was 17.5 GiB live, in 58.6 GiB that its allocator had committed;
- of that, "41.07 freed: kept, or purged and still counted";
- the query storage was 15.3 GiB;
- codegen's heap had peaked at 13.67 GiB.

From the task spans:

- The sum of the peaks of the tasks running together never passed **22.4 GiB**.
- One task peaked at 19.3 GiB. It is a `unit codegen` whose crate the span records as `?`, most
  likely the final blaze binary. It ran for 406 s and was granted the whole 47 GiB.
- From minute 14 to minute 22 that task ran almost alone. Live memory stayed at about 28 GiB
  while the footprint rose from 62 to 75.7 GiB.

### The causes

1. **Freed memory the allocator keeps.** This is the largest share: 46.6 of 75.7 GiB at the
   peak. blaze freed it, but macOS's malloc zone kept the pages. Per-task accounting cannot see
   it, because no task holds it.
2. **One unit too big for the machine.** The final binary's codegen allocates and frees on the
   global heap, far past what it holds at once. That churn is what cause 1 keeps.
3. **Query storage that only grows.** It grew from 3 to 15 GiB over the build and is the
   largest part of the live heap. The front end's releases let go of inference and MIR results,
   but not the memos.
4. **Two measures of memory.** The front-end release triggers on "blaze held" (resident
   memory; 41.12 GiB is its line), while the ceiling and the kernel count footprint. At the end,
   resident read 56 GiB while the footprint read 75.7 GiB, so the release fired late or not at
   all.
5. **No ceiling on the .4 daemon, probably.** macOS refuses `memorystatus_control` on blaze's
   own pid without root or an entitlement. Tree-built daemons log their fallback, a 100 ms
   watch ("held by the daemon's own watch alone"), and the earlier exits at 47, 63 and 89 GiB
   were all tree-built daemons. pid 51782 logs no such line and did not stop. This is inferred
   from the logs, not tested (step 3 tests it).

## What the telemetry could and could not say

The counters and the `serve.log` breakdown were enough to find causes 1 to 4. They were not
enough to name the job, and the evidence was spread across three places:

- **No name on the job.** Task spans carry no `crate` or `unit`, and the biggest one says `?`.
  The worst task had to be identified through its children.
- **Counters have no `build_id`.** A build's counters are found through the daemon's pid.
- **The breakdown by area is text.** The split into query, intern, frames, link, codegen heap,
  store and the others is only in `serve.log`, and only every few minutes. It is not in the
  parquet.
- **Kept memory has no owner.** Nothing says whose frees left 41 GiB committed.
- **`blaze debug profile` fails.** Its 71 MB frame is over the 16 MB frame limit.
- **No ready query.** The timeline above took five hand-written DuckDB queries.

## The plan

Revised 2026-11-09 after the review with Andrew: the root cause is lifetimes mixed on one heap, so
the fix is an owner per lifetime, not releasing kept memory after the fact. `malloc_zone_pressure_relief`
does not reach mimalloc (blaze's global heap), and a release threshold cannot undo pages pinned by
a few long-lived blocks; both are dropped. The rules are in the `memory-and-data` skill. Each step is
one pull request, landed with `.github/scripts/land.py`, telemetry first (`slow-is-p0-traced`).

A plain `blaze check` of blaze (2026-11-09) showed the same shape: 77.7 GiB resident, 36.7 GiB of it
freed but kept by mimalloc, query storage 31.5 GiB, and MIR's heap peaking at 27.6 GiB, so the churn is
in the front end too, not only codegen.

### 1. Telemetry that names the owner

- Counter rows carry `build_id`; the area breakdown in `serve.log` becomes counter rows per tag.
- Task spans name their `crate` and `unit` (and `unit codegen` stops saying `?`).
- Global-heap bytes allocated per task and per call site, from the counting allocator, so the
  question "what churns the global heap" has an answer before anything moves.
- `blaze explain memory [build]`: the timeline, the tasks with the largest peaks, the breakdown at the
  peak. `blaze debug profile` answers in frames under the frame limit.

Built on `heap-telemetry-1109` (crate and unit on task spans, and `allocator kept bytes`, are the
`kept-memory-1109` lane's): counters carry the build under way as `build_id`; the breakdown is
named counters once a second (`crates/blaze-cargo/src/serve/breakdown.rs`); each `task` span ends
with `heap_allocated_bytes` and `heap_freed_bytes`; each tag counts its global-heap churn; the heap
sampler fills `alloc_samples` with one stack per 4 MiB a thread allocates and its call site
(`crates/blaze-profile/src/heap.rs`); and `blaze explain memory` reads them. `blaze debug profile`'s
frame limit is not done yet.

### 2. One measure, one ceiling that holds

Footprint is the one number admission, the front end's release and the ceiling read (rule
`one-memory-measure`). Test the ceiling on the release binary with `BLAZE_DAEMON_MEMORY_GB=4`.

### 3. Scratch on frames, reset per item

- Long tasks reset their frame to a marker after each item: a function in a codegen unit, a body in
  MIR lowering and optimization. A task's peak is one item's scratch plus its outputs.
- What step 1 shows still churning the global heap moves onto the frame at its call site.
- A per-task budget on global-heap bytes fails tests and the self-build, naming the task: the check
  that holds new code to this, where `blaze::implicit_allocator` only judges call sites.

### 4. Outputs as records in the store's buffer pool

Design: [v2/plan/design/store-pool.md](v2/plan/design/store-pool.md).


- `Persist` from one generic impl over `for const` (`#[blaze::reflect]`, `for<F: Field<T>> F::Type:
  Flat`), replacing the hand-written impls (about 20,000 lines in ten `persist.rs` files).
- The store's hot tier as a vmcache-style buffer pool: one reserved address range with a fixed slot per
  page, misses read with explicit I/O through blaze-core (io_uring, kqueue) that the task yields on,
  eviction by the one evictor with `MADV_DONTNEED` / `MADV_FREE_REUSABLE`. Records are immutable, so
  eviction never writes. No mmap of store files for the hot tier (rule `outputs-are-records`).
- Codegen outputs (`CompiledFunction`, object code) and then the query memos move onto it, written once
  by the task that made them.

### 5. Code generation

- Native code from stencils, not Cranelift (rule `proc-macros-on-wasm`); Cranelift's scratch is not
  worth moving onto frames while it is on the way out.
- LLVM (release) allocates through the system malloc, which blaze's accounting cannot see: a context
  per unit, disposed when the unit ends, then `malloc_zone_pressure_relief` for the system zone. Routing
  malloc to mimalloc (its macOS zone override) for accounting comes after step 1 measures how much
  system malloc a release build uses.

## Acceptance

On the Mac, with `BLAZE_DAEMON_MEMORY_GB=48`, a tree-built blaze builds blaze's own tree, and:

- the footprint never passes 47 GiB;
- the daemon never exits 77;
- the Mac does not swap;
- `allocator kept bytes` stays under a quarter of the footprint;
- `blaze explain memory` names the largest task by crate and unit.

The run names the tree's digest and the binary's hash (rule `measure-the-exact-tree`). After
this, the same check runs as a step of the self-build in CI.

## The queries used here

Run from `$BLAZE_HOME/v2/profile` with DuckDB (`blaze debug sql` once step 1 lands).

**The memory timeline, per minute:**

```sql
create table c as
  select t_ns, name, value
  from read_parquet('*.counters.parquet', union_by_name=true)
  where process = 51782
    and name in ('footprint bytes', 'live bytes', 'frame bytes', 'pool running');
create table b as
  select floor((t_ns - (select min(t_ns) from c)) / 60e9)::int m, name, value
  from c;
pivot b on name using max(value) group by m order by m;
```

**The most tasks' peaks held at once** (cast to `bigint`, since the columns are unsigned):

```sql
create table k as
  select start_ns, dur_ns, peak_bytes::bigint p
  from read_parquet('*.spans.parquet', union_by_name=true)
  where build_id = 54 and name = 'task' and peak_bytes is not null;
with ev as (
  select start_ns ts, p d from k
  union all
  select start_ns + dur_ns, -p from k
)
select max(run) / 2^30
from (select sum(d) over (order by ts, d rows unbounded preceding) run from ev);
```
