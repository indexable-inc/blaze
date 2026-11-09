---
status: plan
summary: Why a stage5 self-build's daemon reached 75.7 GiB against a 47 GiB limit on the Mac (memory the allocator kept after blaze freed it, one giant codegen unit, query storage that only grows), and the order of fixes, telemetry first.
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

Each step is one pull request, landed with `.github/scripts/land.py`. The order follows
`slow-is-p0-traced`: the telemetry that names the cause lands before the fix it measures.

### 1. Memory telemetry that answers without hand-written SQL

- Counter rows carry `build_id`, as span rows do.
- The area breakdown in `serve.log` becomes counter rows, one per area:
  `area bytes{area=query|intern|frames|link|codegen|store|...}` and `area heap bytes{area}`,
  sampled with the other counters. The log line stays and reads the same values.
- New counters:
  - `allocator kept bytes`, committed less live;
  - `allocator purged bytes`, cumulative;
  - `query storage bytes{query_kind}`.
- Task spans name their `crate` and `unit`. This is written in the admission lane
  (`admit-footprint-1106`, `8b252bf9fe`).
- `unit codegen` names its crate instead of `?`. This is a separate bug: find where the unit's
  crate name is lost.
- `blaze explain memory [build]` prints the timeline above, the five tasks with the largest
  peaks (with crate and unit), and the breakdown at the peak. It reads only the profile (rule
  `ask-the-profile-first`).
- `blaze debug profile` sends its answer in frames under the limit, so no answer has to fit in
  one frame.

**Done when:** `blaze explain memory 54` (or the same build rerun) prints the cause in one
command: the gap, the task and the area.

### 2. Give kept memory back

This builds memory-pool.md section 7 (macOS) and memory-budget.md section 7.

- **Release before refusing.** Before admission refuses a task, and before the ceiling stops
  the daemon, call `malloc_zone_pressure_relief(NULL, 0)`. Then read the footprint again and
  decide on that reading. This is written in `admit-footprint-1106`; it is a private-ish macOS
  API, so rule `macos-private-apis` applies: a comment saying which API and why, and a test
  that its symbol exists.
- **Release on a high-water mark.** When `allocator kept bytes` passes a quarter of the budget,
  the evictor's thread releases kept memory, never the loop. Its span records the bytes it
  returned.
- **Long term, keep churn off the global heap.** Codegen's working memory moves into a per-unit
  frame (rule `bump-allocate`), mapped so that ending the frame returns it to the kernel
  (`MADV_FREE_REUSABLE` on macOS, an unmap on Linux). The 13.67 GiB codegen heap then leaves
  when the unit ends, with no allocator in between.

**Proof:**

- The admission lane's DST covers release-before-refusing.
- `test-the-cause`: rerun the same tree with the release turned off, and the gap of about 47 GiB
  returns. With the release on, the gap stays under a quarter of the footprint.

### 3. One measure, one ceiling that holds

- The front-end release, admission and the ceiling all read the footprint, the number the
  kernel enforces. "blaze held" as resident memory goes away; ban its name (rule
  `one-concept-one-home`).
- Test the ceiling on the release binary. Start a daemon with `BLAZE_DAEMON_MEMORY_GB=4` on the
  Mac, run a build that needs more, and expect exit 77 within one watch tick past the line.
  Run it against 0.20261105.4 to confirm cause 5, then against the tree. If the release lacks
  the watch, the next release must carry it; it is a ranked blocker for promoting stage 0.

### 4. Split the giant unit

This builds memory-budget.md section 4, codegen that streams.

- The final binary's codegen splits into partitions, each with a predicted peak below a quarter
  of the budget, so admission can schedule them beside other work instead of granting the whole
  budget to one task.
- Each partition's output streams to the store as it finishes. Nothing holds the whole binary's
  objects in memory.

**Done when:** the profile shows no task granted more than half the budget in a blaze
self-build.

### 5. Query storage that can shrink

This builds memory-pool.md sections 3 and 4.

- Use step 1's `query storage bytes{query_kind}` to rank which memos make up the 15 GiB.
- Those memos move onto the pool as evictable hot entries over the store, with the one evictor
  (memory-pool.md section 4). Eviction then reaches the memos, not just the inference and MIR
  results.

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
