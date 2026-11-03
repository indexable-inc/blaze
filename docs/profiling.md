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

To profile any other Rust program (not blaze's builds), use ix-prof and `blaze pprof`, described in
[ix-prof.md](ix-prof.md). Its format is specified in [ix-prof-format.md](ix-prof-format.md).
blaze-profile names sampled addresses with ix-prof's symbolizer (`crates/blaze-profile/src/symbols.rs`).

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
| self_cpu_ns | i64? | the CPU time its job had while it was open, less that of the spans under it: a wait's is about 0, as other jobs run while it parks; null when the binary counts no CPU (`flight::count_cpu`) or it closed under another job. A held span's (one with the field `held`, open across a state machine's steps) counts only while it is entered |
| alloc_bytes | u64? | bytes its thread allocated while it was open |
| allocs | u64? | allocations its thread made while it was open |
| query_kind | dict? | the query it ran, for a span of a query |
| crate | dict? | the crate it was of: a kind's `krate`, or an ad hoc span's field `crate`; a front-end span's names one build of it, `serde v1.0.219 (lib)`, `(lib-test)` for its `cfg(test)` copy, `(lib, wasm)` for the proc macros' wasm copy |
| function | utf8? | the function it was of: a `macro expand`'s macro path, what a `wait` awaits, what a `hold` holds, or an ad hoc span's field `function` or `subject` |
| unit | utf8? | the unit it was of: a unit phase's package, version and target (`serde v1.0.219 (build script)`), or an ad hoc span's field `unit` (a codegen unit) |
| frame_grow_copies | u64? | blocks its thread's task frames moved to grow them while it was open; null when not counted or closed on another thread |
| frame_copied_bytes | u64? | bytes those moves copied |
| awaits | utf8? | what a `wait` waits for: a query's key, a lock, a channel, a scope and the span its caller opened it in; every `wait` has it |
| owner | u64? | who runs what a `wait` waits for (for a key, the job running it, as `switches.job` names it; null when not known), or the job that holds a `hold`. A `hold` span (a key held across a suspension, such as a codegen round) names its holder here, so its waiters' `owner` matches it |
| steps | u64? | the basic blocks a `macro expand` ran on the evaluator; null for a compiled run, a run that did not end, and any other span |
| peak_bytes | u64? | the most stack and heap a `macro expand`'s evaluator held at once; null as `steps` is |
| outcome | dict? | how a `macro expand` ended: ok, panic, step limit, memory limit or error; null for one that did not end (an unwind) |
| waits_on | dict? | the kind of thing a `wait` waits on: key, lock, channel, io, child or scope |
| job | u64? | the pool job a `job` span is of, as `switches.job` names it (or `switches.began_as`, for a job that joined the graph after the span opened); the spans under it are its job's too |
| module_from | dict? | where a `macro module` got its wasm module's machine code: memo (this process had it), store (the daemon's own store) or compiled; null for a load that did not end (refused or failed) |
| code_bytes | u64? | the bytes of a `macro module`'s machine code, as the store keeps it; null for one from the memo |

The columns from `awaits` on are typed: only a span kind fills them, never an ad hoc span's fields. A span kind is a struct declared once in the registry `span_kinds!` at the end of `crates/blaze-profile/src/span.rs`: `unit names`, `unit bodies`, `unit codegen`, `unit link` and `unit store` (`krate`, `unit`), `macro expand` (`path`, `krate`, and as it ends `steps`, `peak_bytes`, `outcome`), `wait` (`awaits`, `function`, `owner`, `on`), `hold` (`what`, `holder`), `job` (`kind`, `job`), `worker` (`doing` in `function`: a pool worker's `search`, `idle` or `switch` between jobs) and `macro module` (`module` in `function`, and as it ends `module_from`, `code_bytes`). Each field names its column by type, so a site that leaves out a field, gives one of the wrong type, or names a column that does not exist does not compile, and a typed column no kind writes fails the build. The kind writes its fields straight into its open span's slot, with no field visitor. `span::read` reads a ring's row back as its kind and `span::read_batch` a `spans` batch; every kind has a view of its rows with its fields as columns, `span_<name>` with spaces as underscores (`span_macro_expand`, whose `path` is `function`). An ad hoc `tracing::info_span!(target: "blaze", ..)` fills `crate`, `function` and `unit` from fields of those names (`krate`, `subject` and `cgu` too); its other fields are kept nowhere.

A `macro expand` span is one run of a proc macro: `function` is the macro's path (`serde_derive::Serialize`), `crate` the crate the call is in. A unit's phase spans (`unit names`, `unit bodies`, `unit codegen`, `unit link`, `unit store`) carry the unit in `unit` as `serde v1.0.219 (build script)`; their `crate` is the target's name, `build-script-build` for every build script. A `scope` wait names the span its caller opened the scope in: `scope 13 of unit codegen serde v1.0.219 (build script)`.

A query span on a body names it: `infer_query`, `infer_anon_const_query`, `mir_body_query`,
`mir_body_for_closure_query`, `body_query`, `body_relative_query` and
`body_with_source_map_query` carry the body's crate in `crate` and its path in `function`. No
two bodies of a crate share a path, so a repeated `function` is a repeated body. The path is
`crate::module::item`, and `crates/hir-def/src/span_names.rs` writes it:
- an impl by its ordinal among its file's impls, `<impl#3>::swap_bytes`, and one a macro wrote also by the call, `<impl#0 in macro call 3fa2.5>::get`, and by each call out to the source file when that call is itself in a macro's output, `<impl#0 in macro call c6a9.0 in macro call 77e1.3>`. Its self type and trait are not in the name, because only its signature has them;
- a block after the body it is in, with its ordinal among that item's blocks:
  `outer::{block#2}::inner`;
- a `const _` by its ordinal among the module's: `_#1`;
- a closure, an `async` block and an anonymous const after their body, numbered by their
  expression in it: `outer::{closure#14}`, `outer::{async#9}`, `outer::{const#3}`; one in a
  signature `Item::{signature const#N}`, in a struct's fields `Item::{field const#N}`;
- a node a macro wrote also by the call: `_#0 in derive macro 3fa2.0`;
- a build script by its package, `build_script[serde]::main` (its `crate` stays
  `build_script_build`);
- `Enum::Variant` for a discriminant.

`hir_def::body_span!` and `span_names::of_inner` write them, and only while a subscriber records
the span. The recorder interns each string, so a row holds a number, and also takes values
recorded after the span began.

Naming a span never changes the query graph. Otherwise a query's reuse would depend on whether the profiler is on. A name reads interned ids, ast ids, module and block identity, the item tree, an impl's or trait's item list and the crate's inputs, and never a signature. Even those reads are edges in blaze-query, so `span_names` reads them inside `blaze_query::db::for_span_text`, which drops them from the running query's frame. That scope returns nothing and writes only into a `String`, so only tracing text leaves it, and `.clippy.toml` refuses any caller but `span_names`. `span_names::tests::naming_a_span_reads_nothing_into_its_query` checks that a query's recorded reads are equal with and without a recording subscriber.

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
| name | dict | what is counted: `memo hits`, `memo misses`, `joined`, `pool queued`, `pool running`, `frame bytes`, `live bytes`, `footprint bytes` (the process's physical footprint, all the OS counts against it; `footprint_per_build` gives each build's peak), `committed bytes` and `reserved bytes` (what every `blaze_mem` region has committed and reserves; footprint less committed is what the process holds outside `blaze_mem`, its global heap), `macro modules from memo`, `macro modules from store` and `macro modules compiled` (each a daemon's running total of its wasm module loads), `profile spans lost` and `profile rows skipped` (the process's running totals of spans the recorder could not record and rows its rings wrote over before the history drained them, written by the history with each build's flush and whenever they grow), and per job on a fiber `fiber stack reserved`, `fiber stack deepest`, `fiber stack touched`, `fiber stack grown` (bytes) |
| t_ns | i64 | when |
| value | f64 | the value then |
| fiber | u32? | the fiber a `fiber stack` counter is of, or null |
| job | u64? | the job that ran on that fiber, or null |

Schema 2 removed `counters.query_kind`: no writer ever filled it. The query engine's counts
are the `query_counts` table.

### `query_counts`: one row a query function a build ran, verified or evicted

| column | type | meaning |
|---|---|---|
| build_id | u64 | the build that ended when they were taken |
| query | dict | the query function, as `spans.query_kind` names a query's span where they agree |
| executed | u64 | its runs |
| verified | u64 | its rows proved unchanged without running |
| reran | u64 | of its runs, those in a view the row had already run in |
| evicted_asked | u64 | of its runs, those for a value evicted earlier and asked for again: what eviction cost |
| evicted | u64 | its values a sweep let go past its cap |
| refused | u64 | of those, the ones recency alone would have kept |
| dropped | u64 | its values an explicit `evict_values` or `evict_values_of` let go |

When a build ends, the daemon takes the counts (`blaze_query::db::take_counts`, in
`crates/blaze-cargo/src/serve/observe.rs`) and the next flush writes them. The counts belong to
the process, so when two builds overlap, the first one to end gets both builds' counts. Hits
are not counted, because a warm hit is four loads with no shared write. `blaze explain
reexecution_per_query_kind` puts the counts beside the spans' runs and distinct function names.
Profiles written before span names were made distinct shared one `function` across bodies
(`<impl NonZero>::swap_bytes` for each integer type, `{block}`, every build script's `main`), so
there a repeated name is not a repeated key.

### `misses`: one row a thing a build computed again instead of reusing, and why

| column | type | meaning |
|---|---|---|
| build_id | u64 | the build that computed it again |
| step | dict | what was computed: `codegen dev template`, `unit`, `build script run` |
| item | utf8 | what of, by path: a function, a unit, a package's build script's run |
| cause | dict | `first`, `parts`, `lost`, `beyond`, `unnamed`, `read`, `unrecorded`, `collected`, `unshaped`, `twice`, `always`, `not_explained` or `failed` |
| what | utf8? | what changed or why, in words: the read, the parts of its key that changed, the reason |
| names | utf8? | the item the change is of, when it is another's: what a chain of reasons follows |
| was | utf8? | the hash of what changed when it was last computed, in hex; null for several parts |
| now | utf8? | its hash now, in hex; null for several parts |
| changed | u32? | how many of its reads or key parts changed; for `twice`, how many keys it had |
| of | u32? | how many reads or key parts it has |
| since | u64? | the last build that computed it and ended well, which it is compared with |

`blaze why` reads the same rows from the daemon (`Request::Rebuilt`). How each cause is worked
out is in docs/v2/plan/lanes/why.md.

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
| event | dict | `started` (joined the graph), `parked`, `ready` (made runnable, on its waker's thread), `resumed`, `ended`, `ran` (a pool job that never joined the graph ended), or a pool worker's `idle` and `woke` |
| job | u64 | the job: its slot's generation << 32 \| its slot; for `ran`, its number before it joined the graph (slot `0xffffffff`); 0 for `idle` and `woke` |
| fiber | u32 | the fiber it runs on |
| core | u32 | the driver's core of the thread |
| wait_kind | dict? | for `parked`: `job`, `key`, `counter`, `until`, or `moved` (the scheduler moving it to another core) |
| wait_value | u64? | the job (as `job` names it), the key's hash, the counter or the deadline; null for `moved` |
| by_job | u64? | for `started` and `ran`, the job that gave it to the pool; for `ready`, the job that woke it |
| since_ns | i64? | for `started` and `ran`, when the job began on its fiber |
| began_as | u64? | for `started`, its number before it joined the graph, which its `job` span and its spawned jobs' `by_job` name until then |
| queued_ns | i64? | for `started` and `ran`, how long the job waited for a core, from its pool's submit to `since_ns`; null when the pool did not say when it was given |

A switch costs one ring row. A job's `ended` row is of the build it last worked for, though its
code let go of the build before it ended.

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

### `events`: one row a `tracing` event the recorder keeps, on the thread it happened on

| column | type | |
|---|---|---|
| build_id | u64? | the build the thread worked for then |
| process | u32 | the process id |
| thread | u32 | the thread it happened on |
| thread_name | dict | the thread's name |
| t_ns | i64 | when |
| job | u64? | the job running on the thread, as `switches.job` names it (its `began_as` before it joined the graph); null on no job |
| level | dict | `ERROR`, `WARN` or `INFO` |
| target | dict | its target: the module path it was written in (`blaze_cargo::serve::daemon`), unless it names one |
| name | dict | its callsite, as `tracing` names it: `event crates/blaze-cargo/src/serve/daemon.rs:1102` |
| message | utf8? | its message, then each other field as `name=value`; null when its text was let go before a reader read it |
| span_id | u64? | the span it happened in, as `spans.span_id`; null outside any |

The recorder keeps an event by the rule it keeps a span by (`blaze_profile::keeps`): INFO or
above, and a target that starts with `blaze` (a `blaze*` crate's `warn!` needs no target). Every
other event costs its call and is never seen, so the lint `blaze::unrecorded_span` finds one
outside a `blaze*` crate and names `target: "blaze"`. An event is one ring row: its message and
fields are interned as one name, so a message that changes each time (a count, a path) costs a
name each time. `blaze debug time --trace` shows each event as an instant on its job's fiber
track (its span's lane when it names no job, its thread's otherwise), and `--html` lists the
build's first 500.

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

### `perf_runs`: one row a profiled run of a program blaze built

| column | type | meaning |
|---|---|---|
| run | utf8 | 16 hex digits: a hash of its repetitions' profiles |
| format | u32 | the perf rows' format: a reader refuses one it does not know |
| target | utf8 | the program, as its profile names it (its executable's file name) |
| started_unix_ns | i64 | when its first repetition began, on the wall clock |
| reps | u32 | how many repetitions |
| tree | utf8? | the digest of the workspace's source tree, as `builds.tree`; null when not known (`gaps` says why) |
| blaze | utf8 | the BLAKE3 hash of the blaze that recorded it |
| binary | utf8? | the BLAKE3 hash of the executable that ran; null when it could not be read |
| build_id | utf8? | the executable's build id, as its profile's frames name it |
| host | utf8 | the machine it ran on |
| arch | utf8 | its architecture |
| os | utf8 | its operating system |
| cpu_period_ns | u64? | the CPU time one sample stands for; null when the sampler was off |
| sampler | utf8 | how the sampler ran, as the profile says |
| samples | u64 | samples of a thread on a CPU, every repetition |
| unmapped | u64 | of those, samples whose running function no code map names |
| wall_ns | i64 | the repetitions' wall time, summed |
| maps | utf8? | the directory of the code maps its rows were named through |
| gaps | utf8? | each input the run lacks, in words, `; ` between them |

### `perf_functions`: one row a function and a repetition of a profiled run

| column | type | meaning |
|---|---|---|
| run | utf8 | the run, as `perf_runs.run` |
| rep | u32 | the repetition, from 0 |
| function | utf8 | the instance's demangled path; the frame's symbol when no code map names it |
| module | utf8? | the image it is in |
| identity | utf8? | what it is across builds (item, which instance, type arguments), hex: what runs are joined by |
| code | utf8? | its own code: its code hash and its type arguments' facts, hex |
| key | utf8? | its code with the codegen settings, hex |
| deep | utf8? | its key and those of everything it refers to, transitively, hex |
| self_samples | u64 | samples with it running |
| total_samples | u64 | samples with it anywhere on the stack, once a sample |
| calls | u64? | times it was called, counted; null when not counted |
| instructions | u64? | instructions it retired, counted; null when not counted |
| gap | utf8? | for a row with no identity: why |

`blaze perf record` writes both, one pair of files a run (`perf-<run>.<table>.parquet`, which the
history's cap never deletes), and `blaze perf diff` compares two runs function by function, joined by
`identity`, with a noise bound and a cause class: docs/v2/plan/lanes/perf-history.md.

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
wait. It fails when a worker spends 1% or more of the wall time outside every span, every run of a
job and every idle stretch (`uncovered_per_worker`; idle is from an `idle` switch to the worker's
next, one begun before the build included). Every job a pool runs opens a `job` span, its
`function` the type of the job's closure (`blaze_graph::pool::span_jobs`). The span moves with the
job and is one row, written on the thread where it closes, so a worker's run of a job counts as
covered from its switch rows: from a `started` or `resumed` switch to the worker's next, and from
a job's begin (`since_ns`) to its `started` or `ran`. The worker's own work between jobs is a
`worker` span: `search` for the next job, `idle` while it sleeps, `switch` for the fiber driver's
part of a park or an end. Before the `worker` spans (2026-11-02) the earlier view counted the time
before a worker's first switch in the build and after its last as uncovered and missed spans begun
on another thread: the 47.9% (self-build b1) and 57.5% (hf b5) it reported were mostly that. On
hf's profile from that day the view below gives 1.1% mean and 3.2% at most for b1, 3.5% and 10.1%
for b5. What is left is stretches with neither a switch row nor a span. On a busy worker they
recurred every flush (`history::EVERY`, 5 s): a self-build's busiest threads wrote 16,379 rows
in one 5 s stretch, a whole ring (`flight::CAP`). Since 2026-11-02 the history drains the rings
every `history::DRAIN` (250 ms) into the next flush, and counts what was still written over in
the `profile rows skipped` counter. It also fails on every perf sample blocked in a mutex, condition
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
  register` (the directory slots of the crates first maintained, one write), `impls nodes` (its
  one node per crate) and in it `impls node` per crate (with `krate` and `predicted_ns`), whose
  `impls warm` fans the crate's reads out and whose `impls crate` (with `krate`) holds the phases
  `impls apply_impls`, `impls rekey`, `impls relang` and the rest, then `impls publish new` for a
  crate first maintained; its apply has `impls publish` per crate;
- `Db::run_all`: `run_all` over the batch and `run_all task` per task, both naming the caller's
  file and line in `function`;
- the name-resolution maintainer's reads per crate: `nameres crate` (its region run to the
  cutoff) and `nameres records`, each with `crate`;
- release codegen: `release summarize`, `release emit` and `llvm compile`, beside the existing
  per-function `mir`, `translate` and `monomorphize`;
- the daemon loop's `loop turn`, opened only for a turn with completions or ready work.

- a build script's run: `build script run`, a root span open from the run's start on the daemon's
  loop to its end (its program's whole time, which no daemon thread spends), with the package
  and version in `unit` (`anyhow v1.0.98 (build script run)`). It is held (the field `held`): it
  is entered only while the loop does a step of the run, so its `self_cpu_ns` is the daemon's
  work for that run alone, and a run whose wall is far above it was its program's time.
- blaze's own toolchain downloaded into a home that lacks it: `toolchain download`, with the
  release in `toolchain` and the bytes in `bytes`. The client downloads it before the daemon
  starts and records no spans, so there it is a step of the client's part of a `BLAZE_PROFILE`
  profile and of the table it prints, `toolchain download nightly-2026-10-27 (116917942 bytes)`.
- the dependencies' image keys: `dependency image keys`, with the crates asked in `crates` and
  the database's view in `function` (`View(1234)`). A serve at the view the same crates were last keyed at, none
  moved, walks nothing and opens no span.

## What made a build slow: `blaze slow`

`blaze slow [--build N] [--top N] [--json]` reads one build's profile (the last build that
ended, unless `--build` says otherwise) and ranks what made it slow by the wall time each fix
would save. A build of 30 s or more ends with a note pointing at it. `blaze debug time` stays
the raw table; `blaze slow` is the verdict. It is a top-level command beside `blaze why`: `why`
says what a build computed again, `slow` says where its time went. It is not `blaze profile`,
because cargo's `--profile` means something else.

Each finding has these fields:
- a kind;
- the fix, in words;
- `saves_ns`;
- how that saving was estimated;
- what it is about (a span name, a wait's class, a query);
- the module or file that owns it, when the spans say (`spans.target`, or a `crates/...rs:N`
  in the subject);
- its evidence, with numbers.

Findings are sorted by saving, then kind, then subject. Ties never depend on hash order, so the
same rows give the same bytes. Findings that save less than 1% of the wall time (and at least
0.2 s) are counted, not listed. The kinds:

| kind | what it finds | estimated saving |
|---|---|---|
| `serial` | critical-path running time, grouped by the innermost span it ran in, while most workers were idle | its path time x (1 - busy workers / workers) |
| `starved` | the same, but the spans' `self_cpu_ns` was under 25% of their running time (1 s or more of running): on a core, or blocked outside any `wait` span, yet getting no CPU | its path time x (1 - CPU / running) |
| `starved_workers` | workers whose CPU per running second is under a third of the median of the top half | the path's time on them x (1 - their rate / the reference) |
| `wait` | waits by class (`report::wait_class`), with the jobs that woke them and what those jobs were running | the path's time while it waited: a wait the path crossed to its waker counts its whole span |
| `queued` | the path's `queued` and `spawned` links: runnable but not run | their time |
| `redone` | one query key (`name`, `crate`, `function`) run more than once in the build | its extra running / mean parallelism |
| `reran`, `evicted` | `query_counts.reran` and `evicted_asked`, costed at the query's CPU per run from its spans | runs x CPU per run / mean parallelism |
| `missed` | the `misses` rows by step and cause, except `first` | unknown |
| `shed` | the daemon's `live bytes` coming into the build (the build before's last value, or this build's first) against the least during it: a fall of more than 1 GiB, or of more than half of 256 MiB or more. What it let go is computed again | unknown |

The estimates are not additive. Each says what fixing that one thing alone would save, so a
wait and the owner's serial work under it can both count the same stretch of the path.

What the profile could not see is listed after the findings, as blind spots, never skipped:
- critical-path time the record lost (`unrecorded` links, by reason and lane);
- job running time inside no span, against the 1% gate;
- worker running time with no span CPU counted for it. No OS CPU is recorded, so the spans'
  total can't be checked against the process's;
- waits with no wait span (rule 13), classed by their kind alone (`key wait with no wait span`)
  and sized by the wall they cover, with the recorder's `profile spans lost` over the build
  beside them; and named waits that never name an owner. A class's evidence says its waits'
  parked time summed over the jobs waiting, which is not workers' time;
- waits the record never ends (`unended_wait`): still parked as it ends, or their `resumed` row
  lost. Each one's parked time runs to the record's end, so they are kept out of the classes
  above (before 2026-11-02, 65,439 of them made one 613 s self-build's `key` waits 4.1 million
  seconds) and sized by the wall with one open;
- no `ready` rows;
- `builds.misses` over the number of misses rows;
- rows that didn't line up;
- a critical path that stops short of the build's start.

The analysis is `blaze_profile::slow::analyse`. It is a pure function over the time model
(`time::Model`), its critical path (`time::path::critical`) and `slow::Facts` (the builds row,
thread names, spans by name and query kind, the misses and query counts, and the memory
counters). `slow::load` is its DuckDB driver. Its tests are fixtures for the hard part: what the
critical path charges to serial work, to a crossed wait and its owner, and to a starved worker.

Both `blaze slow` and `blaze debug time` read a build's switches, spans and events by its
`build_id`, and also the rows with no build inside the build's window. The recorder leaves
`build_id` null on a job's rows once the job's code has let go of the build. On 2026-11-02
that was most of a cold build's switches and spans, and the tagged rows alone showed a tenth of
the work. Since then a pool job works for the build of the job that gave it to the pool
(`blaze_graph::sched::Given::build`), on whichever thread it runs, so a `par::map` a build's
codegen gives the pool is that build's.

## The flight recorder

Every blaze process keeps a fixed ring per thread, holding that thread's recent spans, counter
values, switches and events as `spans`, `counters`, `switches` and `events` rows. Recording allocates nothing and does no I/O. A
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
finds where it began. Its row names the thread that made it: the slot keeps the maker's tid and
name, and a span closed by another thread (or on its own ring after the ring passed to the next
thread) carries them in a column row written as it closes. Until 2026-11-02 the reader named such
a span by whichever thread held the maker's ring when it read, so a pool worker's wait read as the
wait of the next thread to take the worker's ring. A thread has 16,384 slots, allocated 256 at a time as its open spans reach
them: a fiber parked with its enter stack keeps its spans open in the ring of the thread that made
them, so one worker holds the open spans of hundreds of parked jobs. Until 2026-11-02 it had 64,
and most workers of a self-build used every one, losing the spans past them (the waits of the
`key with no awaits` blind spot). A span past the slots is still lost, and counted. Readers take rows with a cursor (`flight::read`): only what is new, and a
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
  fails the request once nothing at all has moved for its bound, 120 s by default).

## The stall watchdog

The watchdog fires on no progress, never on elapsed time. While work is under way (a plane
task runs, or a job a client waits on is not over, whether or not a task of it runs; a job
waiting for its puts to be durable has its own bound and is left out), the daemon reads four
counts, each of which only grows:

| count | what moves it |
| --- | --- |
| jobs ended | a worker pool job finishing (`Pool::ended`); never a worker's idle, search or switch span, nor the daemon loop's own turns |
| queries completed | a query run or verified |
| tasks and steps ended | a plane node settling (a task, or a unit's step), or a job's own effect ending (a placement, a look, its runs' IO) |
| looks that found work running | a turn that finds a worker inside a job's code (`Pool::working`, one long compile) or a run of user code alive |

When none of them has moved for the stall bound, 120 s unless `blaze serve --stall-bound SECS`
says otherwise, the daemon:

1. writes `state/stalls/<time>.json`: the flight recorder's last two minutes and the CPU
   samples as a Chrome trace (ui.perfetto.dev opens it);
2. fails each running request with the error `blaze stalled: no job ended, no query
   completed, no task or step ended and no job's code or run of user code was running for N s
   while M tasks ran (the stall bound is B s,
   ...); it got to <the three counts>`, followed by each thread inside spans, outermost span
   first, with what each awaits, who runs it, and how long it has been in each, then every
   job waiting in the pool's wait graph with the key, counter or job it waits on and that
   key's claimer, then the thread tasks the loop waits on (request, node, name), then each
   job clients wait on with the stage it waits in (its library, plan, fetches, lowering,
   units, runs, snapshot or end) and what it holds (runs, placements, its store);
3. stops the daemon, whose threads may be deadlocked; the next request starts a new one.

A deadlock (jobs that all wait on each other, as in blaze-pool's hang at 0% CPU) becomes this
error within the bound instead of a request that never ends. Closed spans were the first count
and did not work: on 2026-11-02 a lost pool job (poolborrow) sat ten minutes while the loop's
turns and the workers' idle spans kept closing. Tasks running were the first measure of work
under way and did not work either: a job left with every node settled and nothing to move
it (lane seedmem's hang the same day, a blaze without `net` whose fetches all failed) had no
task running, so nothing was watched.

Two faster checks come first. The pool's last sleeper asks the wait graph every 2 s whether
any job's wait can still end; a job on a thread outside the pool (the daemon's main thread,
a client) no longer hides one, and a wait on a key that such a thread claimed counts as stuck
once the daemon says its loop and IO threads are still. Each turn the daemon also asks
whether anything can move the build (`serve/still.rs`, sans IO): with the loop, IO threads and
pool all still, a task posted to the pool with no job parked has lost its job, and fails the
request after 2 s naming the tasks; with jobs parked, it fails once no wait can end, naming
each wait and its key. A step or a span that runs long
while anything else moves is no stall: on 2026-11-02 the earlier per-count bounds (60 s for
spans, 120 s for queries, 300 s for steps) killed self-builds whose `lower workspace` step was
still completing queries. Two warnings go to the log and fail nothing: what every thread is
inside once nothing has moved for half the bound (so a client's own watchdog that stops the
daemon sooner still finds it), and again once one unit's step has run 300 s while no other
ended. A livelock that keeps moving the counts is not the watchdog's to catch: each loop that
waits for progress carries its own bound (fuel, depth or a deadline).

## `BLAZE_PROFILE` and `BLAZE_STATS`

`BLAZE_PROFILE=<dir> blaze build` writes, for every process of the build, its spans with each
`blaze` span's own CPU time. At the end they are merged into `<dir>/trace.json` (a Chrome
trace), `phases.tsv`, `functions.tsv` and `passes.tsv`. `BLAZE_STATS=1` prints the build's wall
time by step.
