---
status: plan
summary: blaze's one profiler (lane observe; a plan until each part it describes lands), one span stream into one buffer with several readers. The schema (spans, counters, builds, samples, provenance, inputs, alloc_samples, heap_by_owner; Arrow, defined once in crates/blaze-profile/src/schema.rs), the flight recorder, the Parquet history with `blaze debug sql` and `nix run .#duckdb`, the live feed (`blaze debug live`, the explorer), the stall watchdog, and `BLAZE_PROFILE` / `BLAZE_STATS`.
date: 2026-10-31
---
# Profiling blaze

blaze has one profiler, its own: one stream of span and counter events, one buffer, and
several readers.

- **The stream.** Every info-level `tracing` span of a `blaze` target, every rust-analyzer
  `*_query` span, and the counters blaze plots (memo hits and misses, the pool's queue, bytes
  of frames and of the heap). On Linux, also the CPU sampler's stacks.
- **The buffer.** The flight recorder keeps each thread's recent events in a fixed ring, in
  the schema below. It is always on, in every build of blaze, and allocates nothing and does no
  I/O per span.
- **The readers.**
  - The history: Parquet files the recorder flushes. `blaze debug sql` runs DuckDB over them.
  - The live feed: Arrow batches every 250 ms over the daemon's socket. `blaze debug live`
    renders them as a TUI, and the explorer at ix.dev/explore draws them.
  - The stall watchdog: it writes the buffer out when a request stops moving.
  - `BLAZE_PROFILE=<dir>`: a whole build's Chrome trace.
  - The hot sketches (crates/blaze-profile/src/hot.rs): each thread's ring also keeps a
    count-min sketch of the time under each (span, `function`) and its 32 heaviest keys, and
    a DDSketch of each kind of wait's durations (1% relative error). `blaze debug hot [--top N]
    [--last-build]` sums the threads' sketches in the daemon and prints the top keys, each
    with an upper bound (the count-min estimate) and a lower bound (ticks counted exactly once
    listed), and p50, p90, p99 and max of each wait kind. No DuckDB, no history.

DuckDB is never linked into blaze. `blaze debug sql` runs the pinned DuckDB CLI.

## The schema

Defined once, in `crates/blaze-profile/src/schema.rs`. The Arrow schema of each table, the
Parquet files, the live feed, the DuckDB views and this page all come from it. Schema version
1. Each Arrow schema's metadata carries `blaze.table`, `blaze.schema` (the version) and
`blaze.clock`.

**Time.** Every `*_ns` instant is in nanoseconds on the machine's monotonic clock: Linux
`CLOCK_MONOTONIC`, macOS `CLOCK_UPTIME_RAW` (`mach_absolute_time`). It is the same clock in
every process on the machine, so the client's, the daemon's and the build scripts' spans line
up as written. The recorder reads the CPU's counter (`CNTVCT_EL0`, `rdtsc`) per event and maps
it to that clock when it writes, calibrating against the clock at both ends of the stretch
written. Durations are nanoseconds.

`dict` is a dictionary-encoded string (`u32` keys). `stack` is a list of `dict` frames,
outermost first. `?` marks a nullable column.

### `spans`: one row a span, from its start to its close, on the thread that made it

| column | type | |
|---|---|---|
| build_id | u64? | the build it was part of; null outside any build |
| commit | dict? | the commit of the workspace the build was of |
| process | u32 | the process id |
| process_name | dict | what the process is: blaze serve, blaze build, a build script's crate |
| thread | u32 | the operating system's thread id |
| thread_name | dict | the thread's name |
| span_id | u64 | unique in its process while the process lives |
| parent | u64? | the span it is inside; null at a root |
| depth | u16 | how many spans its thread was inside when it began |
| name | dict | the span's name |
| target | dict | its tracing target: the module path it was made in, unless it names one (`blaze` for blaze's steps) |
| start_ns | i64 | when it began |
| dur_ns | i64 | how long it was open |
| self_cpu_ns | i64? | the CPU time its job had while it was open, less that of the spans under it: a wait's is about 0, as other jobs run while it parks; null when the binary counts no CPU (`flight::count_cpu`) or it closed under another job |
| alloc_bytes | u64? | bytes its thread allocated while it was open |
| allocs | u64? | allocations its thread made while it was open |
| query_kind | dict? | the query it ran, for a span of a query |
| crate | dict? | the crate it was of: a kind's `krate`, or an ad hoc span's field `crate` |
| function | utf8? | the function it was of: a `macro expand`'s macro path, what a `wait` awaits, what a `hold` holds, or an ad hoc span's field `function` or `subject` |
| unit | utf8? | the unit it was of: a unit phase's package, version and target (`serde v1.0.219 (build script)`), or an ad hoc span's field `unit` (a codegen unit) |
| frame_grow_copies | u64? | blocks its thread's task frames moved to grow them while it was open; null when not counted or closed on another thread |
| frame_copied_bytes | u64? | bytes those moves copied |
| awaits | utf8? | what a `wait` waits for: a query's key, a lock, a channel, a scope and the span its caller opened it in; every `wait` has it |
| owner | u64? | who runs what a `wait` waits for (the job or thread running the awaited key; null when not known), or who holds a `hold`. A `hold` span (a key held across a suspension, such as a codegen round) names its holder here, so its waiters' `owner` matches it |
| steps | u64? | the basic blocks a `macro expand` ran on the evaluator; null for a compiled run, a run that did not end, and any other span |
| peak_bytes | u64? | the most stack and heap a `macro expand`'s evaluator held at once; null as `steps` is |
| outcome | dict? | how a `macro expand` ended: ok, panic, step limit, memory limit or error; null for one that did not end (an unwind) |
| waits_on | dict? | the kind of thing a `wait` waits on: key, lock, channel, io, child or scope |

The columns from `awaits` on are typed: only a span kind fills them, never an ad hoc span's fields. A span kind is a struct declared once in the registry `span_kinds!` at the end of `crates/blaze-profile/src/span.rs`: `unit names`, `unit bodies`, `unit codegen`, `unit link` and `unit store` (`krate`, `unit`), `macro expand` (`path`, `krate`, and as it ends `steps`, `peak_bytes`, `outcome`), `wait` (`awaits`, `function`, `owner`, `on`) and `hold` (`what`, `holder`). Each field names its column by type, so a site that leaves out a field, gives one of the wrong type, or names a column that does not exist does not compile, and a typed column no kind writes fails the build. The kind writes its fields straight into its open span's slot, with no field visitor. `span::read` reads a ring's row back as its kind and `span::read_batch` a `spans` batch; every kind has a view of its rows with its fields as columns, `span_<name>` with spaces as underscores (`span_macro_expand`, whose `path` is `function`). An ad hoc `tracing::info_span!(target: "blaze", ..)` fills `crate`, `function` and `unit` from fields of those names (`krate`, `subject` and `cgu` too); its other fields are kept nowhere.

A `macro expand` span is one run of a proc macro: `function` is the macro's path (`serde_derive::Serialize`), `crate` the crate the call is in. A unit's phase spans (`unit names`, `unit bodies`, `unit codegen`, `unit link`, `unit store`) carry the unit in `unit` as `serde v1.0.219 (build script)`; their `crate` is the target's name, `build-script-build` for every build script. A `scope` wait names the span its caller opened the scope in: `scope 13 of unit codegen serde v1.0.219 (build script)`.

A query span on a body names it: `infer_query`, `infer_anon_const_query`, `mir_body_query`,
`body_query`, `body_relative_query` and `body_with_source_map_query` carry the body's crate in
`crate` and its path in `function`. The path is `crate::module::item`, with `{block}` for a block,
the trait or `<impl Type>` it is in, `Enum::Variant` for a discriminant, and `::{const}` for an
anonymous const in a body. `hir_def::body_span!` writes them, and only while a subscriber records
the span. The recorder interns each string, so a row holds a number, and also takes values
recorded after the span began.

A file's query spans carry its path in `function`: `parse_query` (one parse per file),
`file_item_tree_query` (one per file and crate, the crate in `crate`; a macro file reads
`{macro <id>}`). The same path under several crates is one file lowered again for each;
`crate_def_map_query` carries its crate.

A wait for a query another task is running is a span `wait`, its `awaits` (and `function`) naming the query
it waits for, such as `infer_body_query(Id(1234))`, or a `QueryKey` for the build's own queries.
Its parent is the asking span, whose self time therefore leaves the wait out, so a convoy behind
one slow query reads as many waits naming it. `blaze_graph::pump::Flights::get_or_run_named`
opens it through the hook `pump::span_waits`, which `blaze_profile::install` registers. Jobs it
pumps while it waits are its children. On a fiber, the span moves with the job and closes where
the job resumes. Its `owner` is the task running the awaited key (the runner's job), so a convoy
names both what it waits on and who runs it.

A `Vec` in a frame grows in place while it is the frame's last block. Once something is
allocated after it, it moves when it grows, copied whole. blaze-mem counts those moves per
frame in two `Cell`s, on the slow path only. The frame reports them at task end
(`tag::task_ended`), to the tag's totals (`Counters::grow_copies` and `grow_copied`) and to the
thread's running total (`tag::copied_on_thread`). A span's part is the difference across it,
noted only when the span closes on the thread it began on.
- A span that copied gets one extra ring row after its own, so a span that copied nothing
  costs nothing more.
- The columns are at the end of the table so that adding them keeps the schema version.
- `frame_copies_per_query_kind` ranks query kinds by bytes copied. That ranking decides
  whether blaze needs a second growth frame per worker.

### `counters`: one row a value of a counter at an instant

| column | type | |
|---|---|---|
| build_id | u64? | the build, or null |
| process | u32 | the process id |
| name | dict | what is counted: `memo hits`, `memo misses`, `joined`, `pool queued`, `pool running`, `frame bytes`, `live bytes`, `footprint bytes` (the process's physical footprint, all the OS counts against it; `footprint_per_build` gives each build's peak), and per job on a fiber `fiber stack reserved`, `fiber stack deepest`, `fiber stack touched`, `fiber stack grown` (bytes), and per query kind at a build's end `query executed`, `query verified`, `query reran` |
| t_ns | i64 | when |
| value | f64 | the value then |
| fiber | u32? | the fiber a `fiber stack` counter is of, or null |
| job | u64? | the job that ran on that fiber, or null |
| query_kind | dict? | the query kind a `query` counter is of, as `spans.query_kind` names it, or null |

At a build's end the daemon notes, per query kind, what the query engine counted
(`blaze_profile::query_counts`): its runs, the rows it verified without running, and the runs of
a key that kind had run already in the build. `blaze explain reexecution_per_query_kind` puts them
beside the spans' own runs, distinct keys and repeats, so repeated work shows directly.

The fiber driver notes the four `fiber stack` counters when a job ends, as one row in the
recorder (`blaze_profile::fiber_stack`):
- `reserved` is the fiber's stack, guard page left out.
- `deepest` is how far below the stack's base the job went.
- `touched` is how much of the fiber's stack is committed, which only grows over the fiber's
  life.
- `grown` is the segments the job grew past the fiber's stack.

The `fiber_stacks` view has one row per job, and `jobs_by_stack_high_water` ranks a build's
jobs by `deepest + grown`. A later choice between fibers and a narrow stackless primitive
rests on these numbers.

### `builds`: one row a build request the daemon ran

| column | type | |
|---|---|---|
| build_id | u64 | the build's number, unique in the daemon's store |
| commit | utf8? | the workspace's commit, when it is a git checkout |
| workspace | utf8 | the workspace's root |
| command | utf8 | what was asked: build, test, run, check, with the arguments |
| started_ns | i64 | when the daemon took it |
| dur_ns | i64? | how long it ran; null while it runs |
| started_unix_ns | i64? | when it was taken, on the wall clock |
| outcome | dict | ok, failed, cancelled, stalled or running |
| hits | u64? | nodes the memo settled without running them |
| misses | u64? | nodes that ran their task |
| joined | u64? | nodes that took a task another node ran |
| tree | utf8? | the digest of the workspace's source tree when the build ended, as `blaze-v2-proof tree <dir>` prints it with `target` and `.git` skipped; null for a build of no directory. Compare it with the same digest of a checkout to tell which tree a build tested: `commit` is null for a snapshot that is not a git checkout |
| blaze | utf8? | the BLAKE3 hash of the blaze executable that ran the build |

### `samples`: one row a CPU sample of a running thread

| column | type | |
|---|---|---|
| build_id | u64? | the build of the span its thread was inside, or null |
| process | u32 | the process id |
| thread | u32 | the thread |
| t_ns | i64 | when it was taken |
| stack | stack | the call stack, outermost first, each frame demangled |
| span_id | u64? | the innermost span its thread was inside |
| cpu | dict? | `on` when its thread was on a CPU; `off` when blocked (sampled on macOS), so what it blocked in shows |

On macOS a blocked thread is sampled too, marked `cpu = off`: `raw_blocks` lists the samples of
pool workers blocked in a mutex, condition, `kevent` or `read` with no `wait` span open above
them, each a block that bypasses `blaze_graph::wait`.

### `provenance`: one row a query of a build that was verified or rerun, with the cause

| column | type | |
|---|---|---|
| build_id | u64 | the build |
| query_key | utf8 | the query and its key |
| kind | dict | the query's kind |
| outcome | dict | verified, reran, or reran-same-output (an early cutoff) |
| cause | utf8? | the input or query key whose change made it rerun |
| dur_ns | i64 | how long verifying or running it took |
| unit | utf8? | the codegen unit it was for |
| crate | dict? | the crate it was of |

### `inputs`: one row an input that changed for a build

| column | type | |
|---|---|---|
| build_id | u64 | the build |
| key | utf8 | the input: a path, a variable's name, a flag, a dependency |
| kind | dict | file, env, flag or dependency |
| detail | utf8? | what changed about it |
| seen_by | dict | watcher, rescan or request |

### `alloc_samples`: about one allocation every 512 KiB allocated, on each thread

| column | type | |
|---|---|---|
| build_id | u64? | the build, or null |
| process | u32 | the process id |
| thread | u32 | the thread |
| t_ns | i64 | when |
| bytes | u64 | the bytes the sample stands for (the sampling interval) |
| stack | stack | the call stack, outermost first |
| span_id | u64? | the innermost span its thread was inside |

### `heap_by_owner`: the live heap at the end of a build, by what holds it

| column | type | |
|---|---|---|
| build_id | u64 | the build |
| owner | dict | what holds the bytes: a memo table per query kind, an arena |
| bytes | u64 | live bytes |

### `switches`: one row a switch of a job on a fiber, on the thread it happened on

| column | type | |
|---|---|---|
| build_id | u64? | the build the thread worked for then |
| process | u32 | the process id |
| thread | u32 | the thread it happened on |
| thread_name | dict | the thread's name |
| t_ns | i64 | when |
| event | dict | `started`, `parked`, `resumed` or `ended` |
| job | u64 | the job: its slot's generation << 32 \| its slot |
| fiber | u32 | the fiber it runs on |
| core | u32 | the driver's core of the thread |
| wait_kind | dict? | for `parked`: `job`, `key`, `counter`, `until`, or `moved` (the scheduler moving it to another core) |
| wait_value | u64? | the job's slot, the key's hash, the counter or the deadline; null for `moved` |

The fiber driver (`blaze_graph::sched::fiber::observe`) tells the recorder of each switch, under
its lock, one ring row each. A job's open spans and build move with it:
`blaze_profile::install` registers the recorder's enter stack and build with
`fiber::carry`. A span begun before a wait therefore closes after it, on whatever thread
resumed the job, with the parent and depth it began with. The thread the job left starts its
next job inside no span. Three views read it:
- `job_waits`: one row a wait, its `parked` and next `resumed` switch, with `thread_before`,
  `thread_after`, `waited_ns` and `moved`.
- `job_spans`: `spans` plus the `job` running on the span's thread when it began.
- `job_tiles`, below, is built from `job_spans`.

### `errors`: one row an error a build said: a diagnostic of level error, or each function a refused-functions error names

| column | type | |
|---|---|---|
| build_id | u64? | the build it was part of; null outside any build |
| t_ns | i64? | when the daemon said it |
| crate | dict? | the package of the unit it is of |
| unit | utf8? | the unit as cargo names it: `serde (build script)` |
| kind | dict | a stable code: rustc's (E0277) or a deny lint's name, else blaze's: derive-panic, attr-panic, macro-panic, expand-failed, link-native-missing, link-failed, template-refs, refused, daemon-panic, uncoded |
| function | utf8? | the function or instance it is of: a refused instance, a panicking proc macro |
| message | utf8 | its first line |
| detail | utf8? | why: its first help or note, or a refused function's reason |
| file | utf8? | the file of its primary span |
| line | u32? | its primary span's line, from 1 |

### `tiles`: a result, not stored

The explorer's zoomed-out view: the spans of a time range per (process, thread, depth,
bucket). Its columns are `process u32, thread u32, depth u16, t0_ns i64, t1_ns i64, count u32,
busy_ns i64, name dict` (the name of the longest span in the bucket).

### `job_tiles`: a result, not stored

The per-job lane beside the per-thread one: `tiles` with `job u64` in place of `thread`, from
`job_spans` (the `tiles` op with `"lane": "job"`). A job that moved between threads is one
lane. Its waits are gaps, drawn from `job_waits`.

### `links`: one row an image a build linked

| column | type | meaning |
|---|---|---|
| build_id | u64 | the build |
| image | utf8 | the image: the target as cargo names it, `blaze (bin)` |
| crate | dict? | the package it is of |
| outcome | dict | `patched` (the kept image patched in place) or `placed` (laid out whole) |
| dur_ns | i64? | how long linking it took |

### `crate_deps`: one row an edge of a build's crate graph

| column | type | meaning |
|---|---|---|
| build_id | u64 | the build |
| crate | dict | the crate that depends |
| dep | dict | the crate it depends on |
| kind | dict | `normal`, `build` or `dev`: the dependency table it is from |

The daemon gives both to `History::linked` and `History::crate_graph` (`crates/blaze-profile/src/facts.rs`) and the next flush writes them.

### `open`: a result, not stored

The spans each live thread is inside now, which are not `spans` rows until they close: what the
live feed sends beside the rows that closed. Its columns are `process u32, thread u32,
thread_name dict, depth u16 (0 outermost), name dict, target dict, start_ns i64`, then what each
span is of, as in `spans`: `crate dict?, function utf8?, unit utf8?, awaits utf8?, owner u64?`.
So a stuck worker names the query key it is inside, not only the query's kind.

## Utilization: where a build's wall time goes

The standard utilization query is the view `utilization_per_second`. For each second of a build
it gives the busy threads: the thread-seconds of outermost spans in that second, so 19 is all of
19 threads busy. It also lists the span names they were busy in, each over 10 ms. The build's
window is its `builds` row, and spans count by time, not by `build_id`, so the daemon loop's
spans count too:

```sh
blaze debug sql "SELECT * FROM utilization_per_second"
```

```sql
-- the view's body, from crates/blaze-profile/sql/examples.sql
WITH bounds AS (
    SELECT started_ns AS t0, started_ns + dur_ns AS t1 FROM builds WHERE build_id = $build
), outer_spans AS (
    SELECT name, greatest(start_ns, t0) AS start_ns, least(start_ns + dur_ns, t1) AS end_ns, t0
    FROM spans, bounds
    WHERE depth = 0 AND start_ns < t1 AND start_ns + dur_ns > t0
), seconds AS (
    SELECT unnest(range(0, cast((t1 - t0) / 1e9 AS BIGINT) + 1)) AS second FROM bounds
), by_name AS (
    SELECT second, name,
        sum(least(end_ns, t0 + (second + 1) * 1000000000) - greatest(start_ns, t0 + second * 1000000000)) AS ns
    FROM seconds
    JOIN outer_spans ON start_ns < t0 + (second + 1) * 1000000000 AND end_ns > t0 + second * 1000000000
    GROUP BY second, name
)
SELECT seconds.second,
    round(coalesce(sum(ns), 0) / 1e9, 2) AS busy_threads,
    string_agg(name || ' ' || round(ns / 1e9, 2), ', ' ORDER BY ns DESC) FILTER (WHERE ns >= 10000000) AS busy_in
FROM seconds LEFT JOIN by_name USING (second)
GROUP BY seconds.second ORDER BY seconds.second
```

`uncovered_time` checks the profile itself. It is the share of the build's wall time in which no
thread was inside any span, so the time the profile can't account for; it should stay under 1%.
On 2026-10-31, rel2's release build of blaze-engine (74 s) had 66.9% uncovered; build 8 of
`b2-home-rel`, with the phase spans below, had 0.1% (0.18 s of 134 s).

The gate `blaze_profile::gate::check` holds a profiled build to that bar for each worker and each
wait. It fails when a worker spends 1% or more of the wall time outside every span while it
runs a job (from the job's `started` or `resumed` switch to its next switch) or before its first
switch (`uncovered_per_worker`); the time the pool says it was idle or between jobs counts as
said. Every job a pool runs opens a `job` span, its `function` the type of the job's closure,
closed as the job parks and opened again where it resumes (`blaze_graph::pool::span_jobs`), so
a worker's run of a job is never outside a span. It also fails on every perf sample blocked in a mutex, condition
variable, `kevent` or `read` while no wait span is open on its thread (`raw_blocks`, which names
the stack), and on every `wait` span without `awaits`. Its verdict names the build's `tree` and `blaze`, the exact tree and
binary measured, and a history with no such build is an error naming it. Its test runs on a
profiled self-build named by `BLAZE_PROFILE_FIXTURE` and also fails when that build recorded no
tree or binary.

`critical_path` walks a build's wait graph backwards from the span that ended last. On a lane (a
job, or a thread outside jobs) it steps back to the most recent wait that ended before the
current point. A wait with an `owner` (pump flights, and the `hold` spans of a held key such as a
codegen round) jumps to the job that ran what it waited on; a wait without one stays on its lane.
Each row is one stretch of the path: `ran` time on a lane (named by the span that overlaps it
most) or a `waited` gap. `path_s` is the path's length, `path_pct` its share of the build, and
`parallelism` the threads' busy time divided by the path. The walk is bounded at 100,000 steps;
`complete` is false when it stopped there, so a cut-short path says so. A wait that names no owner hides its
waiter's dependency, so the path is only as complete as the owners are.

Each view whose arguments all have defaults is also a DuckDB view of the same name in every
`blaze debug sql` query, over the last build to end: `SELECT * FROM uncovered_time`. For another
build, `blaze explain uncovered_time build=7`.

The build's top-level phases each have a span:
- the plane's tasks: `std library`, `plan`, `fetch`, `lower workspace` (loading the workspace),
  `select tests`, and the other kinds;
- each unit's compile phases: `unit names` (store reads and the unit's key hash), `unit bodies`,
  `unit codegen`, `unit link` and `unit store` (its store writes), each with the unit's crate in
  `crate`;
- the database's `maintain` and one `maintainer` span per pass, named in `function`;
  `apply_bodies` is the method-resolution delta's body pass; the impls maintainer has `impls
  modules` (its serial walk of every crate's modules), then per crate `impls crate` (with
  `crate`) and its phases `impls apply_impls`, `impls rekey`, `impls relang`, `impls publish`
  and the rest;
- `Db::run_all`: `run_all` over the batch and `run_all task` per task, both naming the caller's
  file and line in `function`;
- release codegen: `release summarize`, `release emit` and `llvm compile`, beside the existing
  per-function `mir`, `translate` and `monomorphize`;
- the daemon loop's `loop turn`, opened only for a turn with completions or ready work.

Running a build script has no span yet.

## The flight recorder

Every blaze process keeps a fixed ring per thread, holding that thread's recent spans and
counter values as `spans` and `counters` rows. Recording allocates nothing and does no I/O. A
span's fields (`crate`, `function`, `unit`, `awaits`) are interned the first time each value
is seen, in generations of 16,384 names a shard: a full one becomes the previous and the one
before it is let go, so a daemon's thousandth build names its keys as its first did. (Until
2026-11-01 the names were capped at 262,144 a process and dropped past it: build 24 of the
round 31 daemon had no `function` on its wait spans and most query spans.)
Next to it, on Linux, a sampler records every running thread's stack 49 times a second
through `perf_event_open`. Frames are named only when they are written out.

Each row is 64 bytes (8 words): close and start ticks, span id, parent, a word packing the kind,
callsite, depth, the ring that made the span and its allocation count, the interned crate and
function, the build and unit, and the bytes allocated. A ring holds 16384 rows (1 MiB). A span's
open state lives from its start to its close in a slot of the ring of the thread that made it.
Its id names that ring, slot and a sequence number, so a span closed on another thread still
finds where it began. Readers take rows with a cursor (`flight::read`): only what is new, and a
reader slower than the writers skips what was written over, counted.

## The live feed

`blaze debug live` and the explorer's live stream each get a `live::Feed` of their own on the
daemon. Every 250 ms it takes what the rings got since its last frame, plus the `open` table,
and sends a frame: `at_ns`, `skipped`, `lost`, then the `spans`, `counters` and `open` tables,
each as one Arrow IPC stream compressed with zstd. A subscriber that can't take a frame on time
misses that tick. Its cursor keeps its place, and rows written over meanwhile show up as
`skipped`, so the daemon holds at most one frame per subscriber. The client folds frames into
`live::Top`, which renders:
- each thread's running spans, outermost first, with how long it has been inside them;
- the ten longest-running spans;
- spans closed per second;
- the memo's hit rate from the last build's counts;
- a warning when nothing has closed for 10 s while threads are inside spans (the watchdog
  fails the request at 60 s).

## The stall watchdog

While tasks are running, the daemon keeps three counts of progress, each with its own bound:

| count | still for | the error says |
| --- | --- | --- |
| spans closed and tasks ended | 60 s | `no span closed and no task ended` |
| queries completed (run or verified) and tasks ended | 120 s | `no query completed and no task ended` |
| units' steps ended | 300 s | `no unit's step ended` |

When one stays still for its bound, the daemon:

1. writes `state/stalls/<time>.json`: the flight recorder's last two minutes and the CPU
   samples as a Chrome trace (ui.perfetto.dev opens it);
2. fails each running request with the error `blaze stalled: <what> for N s while M tasks
   ran`, followed by each thread inside spans, outermost span first, and how long it has been
   in each;
3. stops the daemon, whose threads may be deadlocked or going around without end; the next
   request starts a new one.

A deadlock (jobs that all wait on each other, as in blaze-pool's hang at 0% CPU) becomes the
first error within its bound instead of a request that never ends. Work that keeps closing
spans but never completes a query (a livelock) becomes the second. Queries that keep
completing while no unit's step ends for five minutes becomes the third.

## `BLAZE_PROFILE` and `BLAZE_STATS`

`BLAZE_PROFILE=<dir> blaze build` writes, for every process of the build, its spans with each
`blaze` span's own CPU time. At the end they are merged into `<dir>/trace.json` (a Chrome
trace), `phases.tsv`, `functions.tsv` and `passes.tsv`. `BLAZE_STATS=1` prints the build's wall
time by step.
