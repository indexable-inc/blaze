---
status: plan
summary: blaze's one profiler (lane observe; a plan until each part it describes lands), one span stream into one buffer with several readers. The schema (spans, counters, builds, processes, test_runs, samples, images, provenance, inputs, alloc_samples, heap_by_owner, panics; Arrow, defined once in crates/blaze-profile/src/schema.rs), the flight recorder, the Parquet history with `blaze debug sql` and `nix run .#duckdb`, the client's rows in it, the live feed (`blaze debug live`, the explorer), the stall watchdog, and `BLAZE_PROFILE` as an export from the history.
date: 2026-11-04
---
# Profiling blaze

blaze has one profiler, its own: one stream of span and counter events, one buffer, and
several readers.

- **The stream.** Every info-level `tracing` span of a `blaze` target, every rust-analyzer
  `*_query` span, and the counters blaze plots (memo hits and misses, the pool's queue, bytes
  of frames and of the heap). Beside them, the CPU sampler's stacks (Linux and arm64 macOS), each
  tagged with the span and build its thread was in.
- **The buffer.** The flight recorder keeps each thread's recent events in a fixed ring, in
  the schema below. It is always on, in every build of blaze, and allocates nothing and does no
  I/O per span.
- **The readers.**
  - The history: Parquet files the recorder flushes. `blaze debug sql` runs DuckDB over them.
    A flush writes one file a table, `<build>-<first:08>-<last:08>-<level>.<table>.parquet`
    (an older `<build>-<seq:08>.<table>.parquet` is read too). The history merges a build's files
    as they come, 16 of a level into one of the next (`history::compact::FAN_IN`), and all of a
    build's into one when it ends, by copying column chunks without decoding them: a build keeps
    at most about 15 files a level a table while it runs and one a table after. Before this a
    self-build left 6,175 files.

    **The manifest.** `manifest.json` in the history's directory lists the files a reader reads,
    by table, each with an entry: its schema version, its rows, the range of `build_id` and of
    instants its rows span, the boot those instants count from (Linux's `boot_id`, macOS's
    `kern.bootsessionuuid`), and the process that wrote it
    (crates/blaze-profile/src/history/manifest.rs). Rewriting it (to a hidden name, renamed over
    the last, under `manifest.lock`) is the one commit point of every change: a flush lists its
    files in one commit; a merge lists its output and retires its inputs in one commit; the cap
    retires the oldest builds. So a reader reads each row once, during a merge too: until
    2026-11-04 the views globbed `*.<table>.parquet`, and a glob during a merge could read a
    merged file beside its inputs, every row twice. `blaze debug time`'s derived tables and
    `blaze perf record`'s runs are listed the same way. At open the history deletes what a crash
    left (a file of a form blaze writes that the manifest neither lists nor retires, a hidden
    half-written file) and refuses, naming it, a listed file that is missing or a Parquet file of
    no form blaze writes. A history from before manifests gets one made of its files, once.

    **Pins.** A retired file is unlisted at once and named in the manifest's `retired` list, with
    the manifest's generation that retired it, until it is deleted
    (crates/blaze-profile/src/history/retire.rs). A reader pins the history before it reads the
    manifest (`history::Pin`: a lock file it holds in `pins/`, then the manifest, then a file
    saying the generation it read) and holds the pin until DuckDB has exited. The writer deletes
    a retired file only once every live pin has said that generation or a later one, so no file
    a reader's manifest lists is deleted under it: until 2026-11-04 a merge deleted its inputs at
    once, and a `blaze slow` racing it failed on DuckDB's "Cannot open file". A dead reader's
    lock file, which no one holds, is deleted by the writer.

    **What a reader sees.** Every reader (`blaze debug sql`, `blaze explain`, `blaze slow`,
    `blaze debug time`) first asks the daemon that writes the history to flush
    (`blaze_api::Request::Flush`), and reads after the manifest shows that flush. So it sees
    every row recorded before it asked, each once: a flush moves rows from the recorder to a
    file. `--live` (the recorder's last seconds written beside the history) is gone; it read
    the rows of seconds a flush had also written twice. A reader of one build can read only the
    files that can hold its rows (`sql::Scope::Build`: the build's own, and those of no build
    whose instants overlap its window on the same boot), reading their footers and no other
    file's; `blaze slow` still reads every file, as its recorder counts take the last counter
    before the build.

    **The cap.** Past `history::CAP_BYTES` the history deletes its oldest builds whole, every
    table of a build at once, never one table of a build; files of no build (an idle daemon's
    flushes) go one at a time. A build is as old as its newest flush; the newest flush's build is
    kept.
  - The live feed: Arrow batches every 250 ms over the daemon's socket. `blaze debug live`
    renders them as a TUI, and the explorer at ix.dev/explore draws them.
  - The stall watchdog: it writes the buffer out when a request stops moving.
  - `BLAZE_PROFILE=<dir>`: the build's Chrome trace, exported from the history by the client
    as its request ends (see `BLAZE_PROFILE` below).
  - The hot sketches (crates/blaze-profile/src/hot.rs): each thread's ring also keeps a
    count-min sketch of the time under each (span, `function`) and its 32 heaviest keys, and
    a DDSketch of each kind of wait's durations (1% relative error). `blaze debug hot [--top N]
    [--last-build]` sums the threads' sketches in the daemon and prints the top keys, each
    with an upper bound (the count-min estimate) and a lower bound (ticks counted exactly once
    listed), and p50, p90, p99 and max of each wait kind. No DuckDB, no history.

DuckDB is never linked into blaze. `blaze debug sql` runs the pinned DuckDB CLI over the files the
manifest lists, each table's list led by its `schema.<table>.parquet`.

To profile any other Rust program (not blaze's builds), use ix-prof and `blaze pprof`, described in
[ix-prof.md](ix-prof.md). Its format is specified in [ix-prof-format.md](ix-prof-format.md).
blaze-profile names sampled addresses with ix-prof's symbolizer (`crates/blaze-profile/src/symbols.rs`,
`ix_prof::symbols::functions`): each frame is a function, never `function+0xoffset`, so all of a
function's samples are one frame. Where the executable has DWARF, the functions inlined at an
address are frames of their own, outermost first (the function the symbol names, then each one
inlined into it); without DWARF a frame is the symbol table's function. A return address is named
at the instruction before it (the call). A shared library's frame is its nearest exported
symbol, else `[<file>]`; a live image's slot stub is `stub of <function>`. A proc macro's code,
which Wasmtime compiled into memory no file holds, is named from the module's code map
(`ix_prof::jit`, registered by `blaze-macro-eval` as it loads each module): `<function> [wasm]`
from the module's name section, `wasm function <index> of wasm module <macro>` for a function it
does not name, `code of wasm module <macro> outside its functions` for Wasmtime's trampolines.
Any other address is `[code in no image]`. The names exist only in the process: the raw addresses
are kept in `stack_pcs`, and the `images` table says which file each is in.

## The schema

Defined once, in `crates/blaze-profile/src/schema.rs`. The Arrow schema of each table, the
Parquet files, the live feed, the DuckDB views and this page all come from it. Schema version
5 (`schema::VERSION`; 4 filled `provenance` with the query engine's run log and added
`key_parts` and `query_counts`' causes; at 5 `events.message` became the message alone, the
other fields rows of `event_fields`). How to read all of this when something is wrong: the
`debugging` skill, `.agents/skills/debugging/SKILL.md`. Each Arrow schema's metadata carries
`blaze.table`, `blaze.schema` (the version) and `blaze.clock`, and each file's manifest entry
says its version. A reader reads a file of version 5 as it is, one of version 4 as 5 (its
absent `events.event_id` reads as null: an event with no `event_fields` rows, its fields still
in its `message`), one of version 3 as 5 as well (its absent `query_counts` causes read as
null, not counted; it has no `provenance` rows) and one of version 2 as 5 too (its absent
`folded_ns` also reads as null, which every reader takes as 0; `schema::UPGRADES`), and
refuses any other, naming the file: version 1 named a job by its slot alone, and the
generation version 2 needs was never written.

**Time.** Every `*_ns` instant is in nanoseconds on the machine's monotonic clock: Linux
`CLOCK_MONOTONIC`, macOS `CLOCK_UPTIME_RAW` (`mach_absolute_time`). It is the same clock in
every process on the machine, so the client's, the daemon's and the build scripts' spans line
up as written. The recorder reads the CPU's counter (`CNTVCT_EL0`, `rdtsc`) per event and maps
it to that clock when it writes, calibrating against the clock at both ends of the stretch
written. Durations are nanoseconds.

`dict` is a dictionary-encoded string (`u32` keys). `stack` is a list of `dict` frames,
outermost first. `pcs` is a list of `u64` addresses, outermost first. `?` marks a nullable
column.

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
| self_cpu_ns | i64? | the CPU its thread spent while it was the innermost span the thread was inside (`time::cpu`): a wait's is about 0, as other jobs run while it parks; a span never entered counts 0. Derived from thread CPU read at fiber switches and the other stretch boundaries, not at the span: off from per-span reads by at most its off-CPU time in the stretches it spans, a tick an enter and exit and its duration over 2^23. Null when the binary counts no CPU (`flight::count_cpu`), the span began before its thread's first CPU read, or its stretch's CPU row was never read (`flight::HOLD_MAX`) |
| alloc_bytes | u64? | bytes its thread allocated while it was open |
| allocs | u64? | allocations its thread made while it was open |
| query_kind | dict? | the query it ran, for a span of a query |
| crate | dict? | the crate it was of: a kind's `krate`, or an ad hoc span's field `crate`; a front-end span's names one build of it, `serde v1.0.219 (lib)`, `(lib-test)` for its `cfg(test)` copy, `(lib, wasm)` for the proc macros' wasm copy |
| function | utf8? | the function it was of: a `macro expand`'s macro path, what a `wait` awaits, what a `hold` holds, or an ad hoc span's field `function` or `subject` |
| unit | utf8? | the unit it was of: a unit phase's package, version and target (`serde v1.0.219 (build script)`), or an ad hoc span's field `unit` (a codegen unit) |
| frame_grow_copies | u64? | blocks its thread's task frames moved to grow them while it was open; null when not counted or closed on another thread |
| frame_copied_bytes | u64? | bytes those moves copied |
| folded_spans | u64? | how many of its children were folded into `folded` instead of written (schema 3); null when none |
| folded_ns | i64? | those children's durations, added: time under it that no written child covers but that is not its own (`time::Model` takes it from the span's own running time) |
| awaits | utf8? | what a `wait` waits for: a query's key, a lock, a channel, a scope and the span its caller opened it in; every `wait` has it |
| owner | u64? | who runs what a `wait` waits for (for a key, the job running it, as `switches.job` names it; null when not known), or the job that holds a `hold`. A `hold` span (a key held across a suspension, such as a codegen round) names its holder here, so its waiters' `owner` matches it |
| steps | u64? | the basic blocks a `macro expand` ran on the evaluator, or the instances a `build script proof` walked (`hir_ty_pure::ScriptProof::steps`: the same on a cold, warm or busy daemon, and the margin under the proof's bound); null for a compiled run, a run or proof that did not end, and any other span |
| peak_bytes | u64? | the most stack and heap a `macro expand`'s evaluator held at once (null as `steps` is), or the most a daemon `task` held at once, global allocations and frames together (`blaze_graph::admission::run_as`); null for a task that gave way or panicked |
| outcome | dict? | how a `macro expand` ended: ok, panic, step limit, memory limit or error; null for one that did not end (an unwind) |
| waits_on | dict? | the kind of thing a `wait` waits on: key, lock, channel, io, child or scope |
| job | u64? | the pool job a `job` span is of, as `switches.job` names it (or `switches.began_as`, for a job that joined the graph after the span opened); the spans under it are its job's too |
| creator | u64? | the span (`span_id`) that was open on the thread that gave a `job` span's job to the pool, as it gave it; null for a job given outside any span. A job given by a thread that is no pool job (the daemon's main loop, a build's driver) has no `by_job`, so the critical path crosses from the job to its giver through this span: its lane, and its end as when the job was given (`time::Model::link_creators`) |
| module_from | dict? | where a `macro module` got its wasm module's machine code: memo (this process had it), store (the daemon's own store) or compiled; null for a load that did not end (refused or failed) |
| code_bytes | u64? | the bytes of a `macro module`'s machine code, as the store keeps it; null for one from the memo |
| granted_bytes | u64? | the memory the plane granted a daemon `task` as it started, its predicted peak (`blaze_graph::admission`): against `peak_bytes`, how far off the forecast was |
| passed | u64? | the times a daemon `task` was passed over for memory before it started (another started while its grant did not fit); 0 when memory never held it back |
| node | u64? | the plane node a daemon `task` ran as |
| heap_allocated_bytes | u64? | the bytes a daemon `task`'s work allocated from the global heap in all, never netted against its frees, its fan-out's included (`blaze_graph::arena::Spent`): far past `peak_bytes`, the task churns the heap; null for a task that gave way or panicked |
| heap_freed_bytes | u64? | the bytes a daemon `task`'s work freed to the global heap in all, whoever allocated them; null as `heap_allocated_bytes` is |

The columns from `awaits` on are typed: only a span kind fills them, never an ad hoc span's fields. A span kind is a struct declared once in the registry `span_kinds!` at the end of `crates/blaze-profile/src/span.rs`: `unit names`, `unit bodies`, `unit codegen`, `unit link` and `unit store` (`krate`, `unit`), `macro expand` (`path`, `krate`, and as it ends `steps`, `peak_bytes`, `outcome`), `wait` (`awaits`, `function`, `owner`, `on`), `hold` (`what`, `holder`), `job` (`kind`, `job`, `creator`), `worker` (`doing` in `function`: a pool worker's `search`, `idle` or `switch` between jobs), `macro module` (`module` in `function`, and as it ends `module_from`, `code_bytes`) `build script proof` (`krate`, `unit`, and as it ends `steps`), `admission` (a plane task's wait from ready to start: `kind` in `function`, `node`, and as it ends `waited_for` in `awaits` (memory, a work or io thread, nothing), `passed`, `granted`; detached, on no thread's stack, as no thread waits in it; `blaze explain admission_waits`) and `task` (a daemon plane task run on a thread, `crates/blaze-cargo/src/serve/plane.rs`: `kind` in `function` as `unit bodies (infer)`, `node`, `granted`, `passed`, and as it ends `peak` in `peak_bytes`, `heap_allocated` in `heap_allocated_bytes` and `heap_freed` in `heap_freed_bytes`; `blaze explain task_memory`, `admission_waits` and `memory` read it). Each field names its column by type, so a site that leaves out a field, gives one of the wrong type, or names a column that does not exist does not compile, and a typed column no kind writes fails the build. The kind writes its fields straight into its open span's slot, with no field visitor. `span::read` reads a ring's row back as its kind and `span::read_batch` a `spans` batch; every kind has a view of its rows with its fields as columns, `span_<name>` with spaces as underscores (`span_macro_expand`, whose `path` is `function`). An ad hoc `tracing::info_span!(target: "blaze", ..)` fills `crate`, `function` and `unit` from fields of those names (`krate`, `subject` and `cgu` too); its other fields are kept nowhere.

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
| name | dict | what is counted: `memo hits`, `memo misses`, `joined`, `pool queued`, `pool running`, `frame bytes`, `live bytes`, `footprint bytes` (the process's physical footprint, all the OS counts against it; `footprint_per_build` gives each build's peak), `committed bytes` and `reserved bytes` (what every `blaze_mem` region has committed and reserves; footprint less committed is what the process holds outside `blaze_mem`, its global heap), `macro modules from memo`, `macro modules from store` and `macro modules compiled` (each a daemon's running total of its wasm module loads), `blaze bugs` (the process's running total of bugs it found in itself and went on past, `stdx::assert::reported`; each site is an `errors` row of kind `blaze-bug`), `profile spans lost`, `profile rows skipped` and `profile rows skipped behind` (the process's running totals of spans the recorder could not record, rows its rings wrote over before the history drained them, and of those the rows skipped while the history's backlog was full, written by the history with each build's flush and whenever they grow; `blaze slow` says them on its third line), `span map bytes` and `span map pieces` (the heap bytes and pieces of every macro expansion's span map kept as `parse_macro_expansion`'s value and still alive, `span::kept_maps`, plotted with the memory's plots), `profile cost ns`, `profile spans made` and `profile cpu reads` (the recorder's own measured cost, sampled 1 call in 64, the spans it covers and the thread-CPU reads it made, `flight::cost`, written with each build's flush; `blaze slow` says the cost a span on the same line), and per job on a fiber `fiber stack reserved`, `fiber stack deepest`, `fiber stack touched`, `fiber stack grown` (bytes) |
| t_ns | i64 | when |
| value | f64 | the value then |
| fiber | u32? | the fiber a `fiber stack` counter is of, or null |
| job | u64? | the job that ran on that fiber, or null |

How much of the daemon the kept span maps are, the measure for packing them (fewer bytes a
piece) — their peak against `live bytes`:

```sh
blaze debug sql "select name, max(value) from counters where name in ('span map bytes','span map pieces','live bytes') group by name"
```

`span map bytes` over `span map pieces` is the bytes a piece costs now (a 4-byte end and a
20-byte span, plus fragments and capacity); what a packed layout saves is that less its own.

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
| reads_recorded | u64 | the reads its runs recorded, a row read again later in the same run included: over `reads_kept`, how often its runs re-read the same rows |
| reads_kept | u64 | the reads its runs' rows kept: each run's distinct reads, summed |
| reads_most | u64 | the most reads one of its runs held on its thread's query stack as it ended (the build's most, not a sum) |
| written | u64? | of an input: the times a setter wrote one of its fields, an equal value too (each opens a view): what the build changed, behind every run after it; null in files of a blaze before it. `harness/incremental.py` names a build's re-runs by it |
| ran_new | u64? | of its runs, first runs of a row (schema 4; null before) |
| ran_changed | u64? | of its runs, those because a read changed since the row last ran; `provenance` names the read |
| ran_untracked | u64? | of its runs, those because the row's last run made an untracked read, so it runs in every view; `provenance` names the read |
| ran_cycle | u64? | of its runs, those of a cycle: a stale provisional row, or a verify that re-entered a cycle |

Each run counts its cause once, in `blaze_query`'s `Storage::record_run`, the one place a run
says why it runs, so `ran_new + ran_changed + ran_untracked + ran_cycle + evicted_asked` adds
up to `executed`; a build where it does not has a run path that skips `record_run`, which is a
bug. They are counted always; the per-run rows are `provenance`'s.

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
| cause | dict | `first`, `parts`, `lost`, `unfinished`, `beyond`, `unnamed`, `read`, `unrecorded`, `collected`, `unshaped`, `twice`, `always`, `not_explained` or `failed` |
| what | utf8? | what changed or why, in words: the read, the parts of its key that changed, the reason |
| names | utf8? | the item the change is of, when it is another's: what a chain of reasons follows |
| was | utf8? | the hash of what changed when it was last computed, in hex; null for several parts |
| now | utf8? | its hash now, in hex; null for several parts |
| changed | u32? | how many of its reads or key parts changed; for `twice`, how many keys it had |
| of | u32? | how many reads or key parts it has |
| since | u64? | the last build that computed it (however that build ended), which it is compared with |

`blaze why` reads the same rows from the daemon (`Request::Rebuilt`). How each cause is worked
out is in docs/v2/plan/design/why.md.

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
| build_id | u64 | the build's number, unique in the history: a daemon numbers its builds past every build its files hold rows of |
| commit | utf8? | the workspace's commit, when it is a git checkout |
| workspace | utf8 | the workspace's root |
| command | utf8 | what was asked: build, test, run, check, with the arguments |
| started_ns | i64 | when the daemon took it |
| dur_ns | i64? | how long it ran; null while it runs |
| started_unix_ns | i64? | when it was taken, on the wall clock |
| outcome | dict | ok, failed, cancelled, stalled, running or crashed |
| hits | u64? | nodes the memo settled without running them |
| misses | u64? | nodes that ran their task |
| joined | u64? | nodes that took a task another node ran |
| tree | utf8? | the digest of the workspace's source tree when the build ended, as `blaze-v2-proof tree <dir>` prints it with `target` and `.git` skipped; null for a build of no directory. Compare it with the same digest of a checkout to tell which tree a build tested: `commit` is null for a snapshot that is not a git checkout |
| blaze | utf8? | the BLAKE3 hash of the blaze executable that ran the build |
| process | u32? | the daemon's process id; null in rows of a blaze before it |
| client_started_ns | i64? | when the client process that asked for the build began; null when no client sent its rows (an agent's request, the seed, a blaze before 2026-11-04) |
| client_ended_ns | i64? | when that client sent its rows, at its request's end |
| daemon_user_ns | i64? | the daemon process's user CPU over the build (getrusage `RUSAGE_SELF`, the end less the start); null in the `running` row |
| daemon_system_ns | i64? | the same, system CPU |

A build has two rows: one of outcome `running` and null `dur_ns`, written as it starts (the
history flushes at once), and one as it ends, with the same `started_ns`. The `builds` view keeps
the end row where there is one. A build whose only row is its start, and whose `process` no
longer writes the history (`owner.lock` does not name it, or its lock is not held), ended by
its daemon's death: `blaze slow` reports it to that process's last row.

A daemon whose flight recorder was persisted (see "Past a crash" under the flight recorder)
leaves more: the next history to open, or `blaze slow` when no daemon writes the history,
writes the rows the dead daemon had not flushed, and for each build it was running an end row
of outcome `crashed`, `dur_ns` to the last row read back, `blaze` the hash of the blaze that
ran it and `process` the dead daemon's. `blaze slow` reports a `crashed` build as one whose
daemon died.

**The client's rows.** Only the daemon writes the history. A client that ran a build sends
its own flight recorder's rows (its start-up, connect, printing, a toolchain download) at its
request's end as `Request::ClientRows` (blaze-api schema 38), every row tagged with the build
id the daemon gave it in `Worked::build_id`, with its start and end on the history's clock.
The daemon writes them into `spans`, `events` and `folded` as the client's process
(`process_name` = `blaze client`, `pid` the client's) and refuses rows of another build. The
times go on a third row of the build in `builds` with `client_started_ns` and
`client_ended_ns`; the view keeps the row with `dur_ns` and those times. A client's rows that
arrive before the build's end row are held and go on it. The client waits for the write (at
most the loop's 50 ms idle tick), so an export right after reads them. `blaze debug time`
reads one process's rows a build, the daemon's, so it leaves the client's out.

### `processes`: one row a child process blaze spawned, reaped

Written as the driver reaps the child (blaze-core `process.rs`, through `blaze_core::reaped`
into `facts::reaped`), from its rusage. A spawner names the child before its spawn effect goes
to the driver (`blaze_core::reaped::label`); a spawn no one named has kind `unlabelled`.

| column | type | |
|---|---|---|
| build_id | u64? | the build it worked for, as the daemon's recorder said when it was labelled; null for none |
| process | u32 | the daemon's process id |
| pid | u32 | the child's process id |
| kind | dict | `build_script`, `test`, `run`, `warm_server`, or `unlabelled` |
| unit | utf8? | what it ran for: `anyhow v1.0.98 (build script run)`, a test binary, a program |
| program | utf8 | the executable |
| parent_span | u64? | the span it was asked for inside; null for none |
| spawned_ns | i64 | when the driver began to start it, before its sandbox |
| exec_ns | i64? | when the spawn returned, the child having exec'd |
| exited_ns | i64 | when it was reaped |
| sandbox_ns | i64? | how long making its sandbox took; null for a child run without one |
| user_ns | i64 | its user CPU |
| system_ns | i64 | its system CPU |
| peak_rss | u64? | its peak resident set, in bytes (Linux reports KiB, macOS bytes; both written as bytes) |
| minor_faults | u64? | page faults served without IO |
| major_faults | u64? | page faults that read from disk |
| voluntary_switches | u64? | times it gave up the CPU to wait |
| involuntary_switches | u64? | times the kernel took the CPU from it |
| code | u32? | its exit code; null when a signal ended it |
| signal | u32? | the signal that ended it |

Not covered yet: children the threads driver's `run_process` starts (unlabelled spawns are
written, but with no unit), children a test's fork server forks, and Windows.

### `test_runs`: one row a test that finished, from the test queue's `Finished` events

| column | type | |
|---|---|---|
| build_id | u64 | the build that ran it |
| test | utf8 | the test's path |
| crate | dict? | the package its binary is of |
| binary | utf8 | the test binary |
| outcome | dict | `passed`, `failed`, `ignored`, `timed_out` or `crashed` (a signal, or a lost process) |
| dur_ns | i64 | how long its last attempt ran |
| retries | u32 | attempts after the first |
| ended_ns | i64 | when it finished |

### `samples`: one row a CPU sample of a running thread

| column | type | |
|---|---|---|
| build_id | u64? | the build of the span its thread was inside, or null |
| process | u32 | the process id |
| thread | u32 | the thread |
| t_ns | i64 | when it was taken |
| stack | stack | the call stack, outermost first, each frame a function (inlined ones their own frames) |
| span_id | u64? | the innermost span its thread was inside |
| cpu | dict? | `on` when its thread was running or runnable as it was sampled, `off` when blocked inside a span, so what it blocked in shows; the same on Linux and macOS |
| stack_pcs | pcs? | the stack's addresses, outermost first, one a frame of the unwind (an inlined function's frames in `stack` share one): the last is where the thread was, each other a return address, so the call is the address before it; null in rows of a blaze before it |
| job | u64? | the job its thread was running as it was sampled, as the profiler numbers jobs (`switches.job`); null for none, and in rows of a blaze before it |
| fiber | u32? | the fiber that job ran on; null for none, and in rows of a blaze before it |

Both samplers take 49 samples a second of each thread that records spans (holds a ring): one
running or runnable, `cpu = on`, and one blocked inside a span, `cpu = off`. A blocked thread
outside every span is not sampled: no span could be blamed for its wait. `on` means the same on
both systems: the thread was running or runnable (on Linux, a sample of its task clock, or a
preemption; on macOS, `TH_STATE_RUNNING`, which counts a runnable thread too). `raw_blocks` lists the `off`
samples of pool workers with no `wait` span open above them, each a block that bypasses
`blaze_graph::wait`.

- **Linux** (`crates/blaze-profile/src/sampler/linux.rs`). No signal is sent to any thread
  (rule `deadline-waits`; a signal every 20 ms kept `io_uring_enter` from ever timing out and
  hung `blaze serve` 0.20261105.0 before it listened). The sampler's thread opens one
  `perf_event_open` event for each such thread: its user-only task clock at 49 Hz, with user
  call chains the kernel walks by frame pointer, times on `CLOCK_MONOTONIC`, and the kernel's
  context-switch records, read from the event's mapped ring every 20 ms (the one perf reader,
  `crates/ix-prof/src/perf.rs`). Perf's samples arrive after the fact, so each flight ring
  keeps a context log, the span, job and fiber its thread was in from each instant (written at
  every span enter and exit and every fiber switch), and a sample takes the entry in force at
  its time; a span that has closed since gives way to its innermost ancestor still open. A
  switch-out that is not a preemption, inside a span, becomes `cpu = off` samples weighed by how
  long it blocked, with no stack: the kernel's stack of a block is a kernel-mode sample, which
  `perf_event_paranoid` 2 (the default here) refuses, so the span says where it blocked. It
  needs `perf_event_paranoid` 2 or lower, or `CAP_PERFMON`; where perf is refused, the sampler is
  off and its state names the error and the setting. There is no fallback.
- **macOS** (arm64, `sampler/macos.rs`). Each tick the sampler stops each such thread
  (`thread_suspend`), reads its registers, walks its frame pointers inside its stack, reads its
  span and build, and lets it go.

### `images`: one row a code image of a process that `stack_pcs` addresses are in

| column | type | |
|---|---|---|
| process | u32 | the process id |
| boot | dict? | the machine's boot it ran in (Linux's boot_id, macOS's boot session UUID): a process id is unique only within one; null for a recovered fatal signal's executable (see `panics`) |
| seen_ns | i64 | when it was written |
| path | utf8 | the file it was loaded from |
| build_id | utf8? | its GNU build id (Linux) or Mach-O UUID (macOS), hex; null when it has none |
| blake3 | utf8? | the BLAKE3 of its file, hex; null when it was not hashed, `unhashed` says why |
| unhashed | utf8? | why `blake3` is null: the file is gone, changed (its build id no longer matches) or not on disk (macOS's shared cache) |
| base | u64 | the address its code starts at in the process |
| size | u64 | the bytes of code from `base`: an address in [base, base + size) is in it |
| slide | u64 | how far it was loaded from the addresses its file gives: a file address is an address less this |
| names | dict | how blaze named its frames: dwarf (functions and inlined ones), symbols (no DWARF), exports (a shared library's exported symbols) or none |

A history writes an image the first time a flush's samples hold an address in it, once a
process. To name a `stack_pcs` address after the process is gone: find the row of its `process`
and `boot` whose `[base, base + size)` holds it, take the file by `blake3` or `build_id`, and
look up `address - slide` (less 1 for every address but the last of the stack) in it. An
address in no image is JIT code (a proc macro's wasm, named only in the process, in `stack`) or
a wild frame.

### `provenance`: one row a query run that was not a first run, with why it ran

The query engine's run log (`blaze_query::db::take_log_in`), on while the daemon keeps a history
(`blaze serve`) and taken as a build ends beside `query_counts`. One row a run that was not a
first run (a cold build's runs are `query_counts.ran_new` alone, so a cold build writes few
rows), and one row the first untracked read of a run. Verified rows are counted, not listed.
Rows are sorted by query, key, view and cause, so two runs of one build write the same rows.
Past 2^19 entries between two builds' ends the log keeps the first and the daemon warns with
the number dropped; the counts still hold every run. Before schema 4 nothing wrote this table.

| column | type | |
|---|---|---|
| build_id | u64 | the build that ended when the log was taken |
| query_key | utf8 | the memo: its query and its key as the key's type shows it, `` `infer_query`(DefWithBodyId(..)) `` (its id where the key has no `Debug`) |
| kind | dict | the query, as `query_counts.query` names it |
| outcome | dict | `ran`, or `untracked read` (its run made a read the engine can't see) |
| cause | utf8? | for `ran`: `changed`, `evicted`, `untracked` or `cycle` |
| view | u32 | the database view it ran in: each write of inputs opens one |
| cause_key | utf8? | for `changed`: the read that changed, `` `Input.field`(<row>) `` for an input's or tracked struct's field (a file's text by its path, `` `FileText.text`(/src/lib.rs) ``; another row by its id), else a memo named as `query_key` names it, whose own row says why it changed |
| cause_kind | dict? | what `cause_key` is: `input`, `tracked`, `interned` or `function` |
| what | utf8? | for `untracked read`: its name, the `what` of `Untracked { what }` |

Each storage keeps its own log as node keys, so logging formats nothing; the build's front end
takes it as the build ends, with its database attached (`blaze_query::db::take_log_in`, through
`Writer::ran`), and only then is each node named: a memo by its key's `Debug`, an input's row
by the front end's namer (`blaze_query::db::name_rows`; base-db names `FileText` by path),
else by id. `blaze explain what_reran`, `why_reran` (each rerun followed
through the memos that changed to its root), `untracked_reads` and `cycle_reruns` read it.

### `key_parts`: one row a part of the key of a thing a build computed again

The parts of each `misses` row's key (`blaze_kernel::Miss::parts`), written beside it, so two
builds' keys, or two homes' histories, can be diffed part by part (`blaze explain
key_part_diff before=N after=M`). A key with no parts has one row with a null `part`.

| column | type | |
|---|---|---|
| build_id | u64 | the build that computed it |
| step | dict | as `misses.step`: `unit`, `build script run`, `codegen dev template` |
| item | utf8 | as `misses.item` |
| key | utf8 | the whole key, in hex, as the store files it |
| part | utf8? | the part's name: `toolchain`, `settings`, `env CC`; null for a key with no parts |
| hash | utf8? | the part's hash, in hex; null when the key has no such part this time |
| names | utf8? | the item the part is of, when another's (a dependency's unit) |
| beyond | utf8? | what the value is filed under past the key that the parts do not explain (a unit's process variables, a build script's traced reads) |

### `inputs`: one row an input that changed for a build

| column | type | |
|---|---|---|
| build_id | u64 | the build |
| key | utf8 | the input: a path, a variable's name, a flag, a dependency |
| kind | dict | file, env, flag or dependency |
| detail | utf8? | what changed about it |
| seen_by | dict | watcher, rescan or request |

### `alloc_samples`: one global-heap allocation in every 4 MiB a thread allocates

The daemon's heap sampler (`crates/blaze-profile/src/heap.rs`, started by `blaze serve`): every
`blaze_graph::arena::SAMPLE_EVERY` (4 MiB) bytes a thread allocates through the counting
allocator, the allocation that crosses it is sampled, its frame pointers walked inside the
allocator into a fixed ring of atomics (no lock, no allocation) and named at the flush, as the
CPU samples are. Summed by `site`, the samples' `bytes` are what was allocated there, within a
sample's: what churns the global heap, by function (`blaze explain memory`). A sample written
over before a flush read it is counted in the flush's skipped rows.

| column | type | |
|---|---|---|
| build_id | u64? | the build its thread worked for, or null |
| process | u32 | the process id |
| thread | u32 | the thread |
| t_ns | i64 | when |
| bytes | u64 | the bytes of allocation the sample stands for: the interval, or as many of it as a larger block crossed |
| stack | stack | the call stack, outermost first |
| span_id | u64? | the innermost span its thread was inside |
| stack_pcs | pcs? | as in `samples` |
| tag | dict? | the blaze-mem tag of the task that made the block; null outside any task |
| block_bytes | u64 | the sampled block's own bytes |
| site | utf8? | the innermost frame of `stack` outside the allocator and the collections that call it (`alloc::`, `core::`, `std::`, `hashbrown::`, ...) |

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
| cpu_ns | i64? | the thread's CPU clock read at the switch (`time::cpu`): the difference of two of a thread's rows is the CPU it ran between them. Null for `ready`, when the binary counts no CPU, and before 2026-11-04 |

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

### `folded`: one row a sum of short spans of one site, on one thread, over a stretch

A span the recorder closes in under `flight::FOLD_BELOW` (20 us) is summed instead of written when
no row could name it: it has no child, no event or sample happened in it, no job was given in it,
it fills no typed column, it was never held across a wait or moved to another thread, and it made
no frame copies. A `wait` span is folded only under `flight::WAIT_FOLD_BELOW` (1 us; of build 24's
164,892 waits under 1 us none parked, while 42,532 of the 96,076 from 1 to 5 us did), and a `job`
or `hold` span never. The parent keeps exact totals: its row's `folded_spans` and `folded_ns` say
how many children were folded and their time, so nothing is dropped. A thread sums by site,
parent site, depth 0 or not, and build, in 128 slots (4 probes from a key's home), and writes a sum
as a row when its slot is needed for another, when it has been open `flight::FOLD_AGE` (50 ms),
when the worker goes idle, when its ring passes to another thread, and when a build ends. Only
blaze's CLI and daemon turn folding on (`flight::fold_below`, called in `blaze_cargo::cli::main`;
until schema 3 nothing called it and every span was written); a test records every span unless it
asks. On build 24 of the s05b home, 133 million of 154 million tagged spans were under 20 us and
fold into about 1.1 million rows (measured from that build's spans; estimated, not yet recorded).

The view `span_sums` is `spans` (one run a row) and `folded` (`spans` runs a row) under one shape
(`runs`, `dur_ns`, `max_dur_ns`, `self_cpu_ns`, `allocs`, `alloc_bytes`, `root`, `query_kind`):
count or time spans by name or query kind from it, not from `spans` alone.

| column | type | |
|---|---|---|
| build_id | u64? | the build its spans were of |
| process | u32 | the process id |
| thread | u32 | the thread that closed them |
| thread_name | dict | its name |
| name | dict | their callsite's name, as `spans.name` |
| target | dict | their callsite's target, as `spans.target` |
| root | u32 | 1 when they were roots (depth 0: a worker's own work between jobs), else 0 |
| from_ns | i64 | when the first of them began |
| to_ns | i64 | when the last of them closed |
| spans | u64 | how many |
| dur_ns | i64 | their durations, added |
| max_dur_ns | i64 | the longest |
| self_cpu_ns | i64? | their own CPU, added, where the thread counts CPU |
| alloc_bytes | u64? | the bytes they allocated, added |
| allocs | u64? | the allocations they made, added |
| parent_name | dict? | their parent's callsite name; null at a root |
| parent_target | dict? | their parent's callsite target |

### `events`: one row a `tracing` event the recorder keeps, on the thread it happened on

| column | type | |
|---|---|---|
| build_id | u64? | the build the thread worked for then |
| process | u32 | the process id |
| thread | u32 | the thread it happened on |
| thread_name | dict | the thread's name |
| t_ns | i64 | when |
| job | u64? | the job running on the thread, as `switches.job` names it (its `began_as` before it joined the graph); null on no job |
| level | dict | `ERROR`, `WARN`, `INFO` or `DEBUG` |
| target | dict | its target: the module path it was written in (`blaze_cargo::serve::daemon`), unless it names one |
| name | dict | its callsite, as `tracing` names it: `event crates/blaze-cargo/src/serve/daemon.rs:1102` |
| message | utf8? | its message alone (`tracing`'s `message` field); null when it has none or its text was let go before a reader read it |
| span_id | u64? | the span it happened in, as `spans.span_id`; null outside any |
| event_id | u64 | unique in its process for the process's life (its ring's number << 48, then its row's place in that ring): what its `event_fields` rows name it by, with `process` |

### `event_fields`: one row a field of an event other than its message, typed as the event recorded it

| column | type | |
|---|---|---|
| build_id | u64? | its event's build |
| process | u32 | the process id |
| event_id | u64 | its event, as `events.event_id` |
| ordinal | u16 | its place among its event's fields, in the order the event recorded them, from 0 |
| key | dict | the field's name, as its callsite writes it |
| kind | dict | which value column holds it: `i64`, `u64`, `f64`, `bool` or `str` (a `&str`, or anything recorded through `Display` or `Debug`, as text) |
| value_i64 | i64? | for `i64`: a signed integer (an `i128` that fits) |
| value_u64 | u64? | for `u64`: an unsigned integer (a `u128` that fits) |
| value_f64 | f64? | for `f64` |
| value_bool | bool? | for `bool` |
| value_str | utf8? | for `str`: its text; null when it was let go before a reader read it |

Two views over them: `event_values` adds `value_int` (an `i64` or `u64` as one HUGEINT) and
`value_text` (any value as text) to each field, so a filter need not know a field's kind; and
`events_wide` is `events` with `fields` (every field as `key=value`, in order, the text the
message held before version 4) and the keys blaze's events say most as columns of their own:
`unit`, `image`, `path`, `dir`, `what`, `why`, `error`, `outcome`, `step` (text) and `bytes`
(an integer). A history written before version 4 has no `event_id` and no `event_fields`: its
fields are in its events' `message`.

```sql
-- every event of build 12 that names the unit serde, with its fields
SELECT t_ns, level, message, fields FROM events_wide WHERE build_id = 12 AND unit = 'serde';
-- the events whose field bytes passed 1 MiB, of any key's kind
SELECT e.message, v.key, v.value_int FROM events AS e
JOIN event_values AS v ON v.process = e.process AND v.event_id = e.event_id
WHERE v.key = 'bytes' AND v.value_int > 1048576;
```

**What is kept** (`blaze_profile::logs::keeps_event`, the recorder's one rule for events):

| level | kept in the ring |
|---|---|
| ERROR, WARN | every event, of any target: a dependency's warning is seen too |
| INFO | a target that starts with `blaze` (a `blaze*` crate's `info!` needs no target), as spans are kept (`blaze_profile::keeps`) |
| DEBUG | only a target that is `blaze` or starts with `blaze::` (`debug!(target: "blaze::store", ..)`): an opt-in, so a `blaze*` crate's `debug!` under its module path costs no row in a hot path |
| TRACE | never; release builds compile it out (`tracing`'s `release_max_level_debug`) |

Every other event costs its call and is never seen, so the lint `blaze::unrecorded_span` finds
an INFO one outside a `blaze*` crate and names `target: "blaze"`.

**What the history writes** (`blaze_profile::logs::Hold`, run on each flush's rows before they
are written). Every ERROR, WARN and INFO row is written. A DEBUG row is written by the policy
`BLAZE_HISTORY_LEVEL` names, read once as `blaze serve` starts (a value that is neither stops the
daemon, naming it):
- `info` (the default): a build's DEBUG rows are held in the daemon until the build ends, and
  written when it failed or stalled, when an ERROR event of that build was read, or when the
  history closes before the build ended (the daemon stopping under it). They are dropped when it
  ended ok or cancelled; rows of no build are dropped. At most 16,384 DEBUG events a build and 8 builds
  are held, the oldest going first. The flush's `Flushed` counts `events_held` and
  `events_dropped`.
- `debug`: every DEBUG row the ring kept.

The rule reads only the rows and the builds' ends in the order the history was told them, never
a clock, so the same build writes the same rows. A daemon that crashes loses what it held; the
ring still has the build's recent DEBUG rows, which a stall dump (`flight::dump`) reads; a
persisted recorder (see "Past a crash") recovers only what the history had not flushed, so held
DEBUG rows already drained are not recovered either.

An event is one ring row (its message interned) and a row for every five fields after it, each
value a word: a number is kept as itself with no allocation, and text is interned (a `Debug`
value formatted once into a thread's buffer). Up to 32 fields an event are kept; past them, and a
field whose `Debug` itself logs, are counted (`flight::fields_lost`). `blaze debug time --trace`
shows each event as an instant on its job's fiber track (its span's lane when it names no job,
its thread's otherwise), its fields typed in its args beside its build, job, span and id, and
`--html` lists the build's first 500 with their fields.

The `BLAZE_PROFILE` export is the same trace, so its `trace.json` shows the daemon's events as
instants too (a client's events are in the history's `events` but not in that trace).

Each link says its image's shape as events (crates/blaze-link/src/shape.rs), once a stage after
its layout: `link region`, one a region with its `addr`, `size` (bytes placed), `capacity` (the
range kept), `section` and `class`; and `link code`, the code's pieces and bytes `by=backend`
(stencil, cranelift, llvm, native, linker, unsaid) and `by=crate` (the 16 largest, then the
rest). `blaze explain link_regions` lists each image's regions by address with the bytes unused
before each, and `blaze explain link_code` its code by backend and crate, both from the events'
typed fields (`event_values`). A link refused for
reach or room (`OutOfReach`, `NoRoom`, a field a value does not fit) shows the same regions in
its message, the reach's two ends marked.

### `errors`: one row an error a build said: a diagnostic of level error, or each function a refused-functions error names

| column | type | |
|---|---|---|
| build_id | u64? | the build it was part of; null outside any build |
| t_ns | i64? | when the daemon said it |
| crate | dict? | the package of the unit it is of |
| unit | utf8? | the unit as cargo names it: `serde (build script)` |
| kind | dict | a stable code: rustc's (E0277) or a deny lint's name, else blaze's: derive-panic, attr-panic, macro-panic, expand-failed, link-native-missing, link-failed, template-refs, refused, daemon-panic, blaze-bug (a site of a bug the daemon went on past: `function` its value kind, `message` what and how often, `detail` the first report's thread and stack), uncoded |
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
`identity`, with a noise bound and a cause class: docs/v2/plan/design/perf-history.md.

### `open`: the spans live threads are inside

The spans each live thread is inside when the threads were read, which are not `spans` rows
until they close: what the live feed sends beside the rows that closed, and what each flush of
the history writes when it differs from the last one written (an idle daemon writes none, a
deadlocked one keeps its last). Its columns are `process u32, thread u32, thread_name dict,
depth u16 (0 outermost), name dict, target dict, start_ns i64`, then what each span is of, as in
`spans`: `crate dict?, function utf8?, unit utf8?, awaits utf8?, owner u64?`, then `read_ns i64`,
when the threads were read (one value a reading), then `task u64?`, the nearest task among the
span and its ancestors (a daemon `task` span's plane request `<< 32 |` its node, the id of the
explorer's `tasks` row; added at the end, so the schema's version stays 5). So a stuck worker names the query key it is
inside, not only the query's kind, and the history of a daemon that died says what it was
inside at most one flush (5 s) before its end: `blaze slow` reads it for the build the death
ended, and the CLI a dead daemon hung up on quotes it.

### `panics`: one row a panic, or a fatal signal a daemon died of

Every panic of a daemon, caught or not, and each fatal signal (SIGSEGV, SIGBUS, SIGILL, SIGFPE,
SIGABRT) a blaze daemon died of, with the stack of the thread it was on. One table for both,
told apart by `kind`: a fatal signal is a panic the process could not survive, the questions
asked of them are the same (which thread, inside which spans, at which frames, in which build),
and an abort a panic led to is one row naming both.

| column | type | meaning |
|---|---|---|
| build_id | u64? | the build it was in: the thread's job's, or its innermost span's |
| process, thread | u32 | the process and the OS thread id |
| thread_name | dict | std's name for the thread, else the system's |
| t_ns | i64 | when, on the schema's clock |
| kind | dict | `panic` or `signal` |
| job, span_id | u64? | the job running on the thread, and its innermost open span |
| message | utf8? | the panic's text, cut at 1 KiB with `...`; a signal's name and meaning (and the fault address for SIGSEGV and SIGBUS), then the thread's last panic, if it had one |
| location | utf8? | `file:line:column` the panic was raised at (a signal: its thread's last panic's) |
| stack | list<dict> | the frames, outermost first, each named by function (with those inlined there) as `samples.stack` |
| stack_pcs | list<u64> | the stack's addresses, outermost first, one a frame of the unwind, so they can be named again later |
| caught | u32 | 1 when a job's boundary caught it (that job failed, the build went on) |
| open_spans | list<dict> | the spans the thread was inside, outermost first |
| signal | dict? | `SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGFPE` or `SIGABRT`; null for a panic |
| fault_addr | u64? | the address the signal was about |
| boot | dict? | the machine's boot the process ran in; with `process`, the key of the `images` rows that name `stack_pcs` (`base <= pc < base + size`); null for a signal |

A panic's addresses are in the `images` rows its flush wrote for the process (with the samples'),
and a recovered signal's in an `images` row its recovery writes for the dead process's executable
(this one's file, which the record's BLAKE3 matched, at the dead process's slide, with no boot):
join on `process`, `boot IS NOT DISTINCT FROM` and the address range.

A panic is recorded by the process's one panic hook (`blaze_profile::panics::record`, which the
daemon installs through `plane::record_panics`) into a fixed ring of 64 slots, with no
allocation and no lock: a seqlock of atomics, its stack walked up the frame pointers with each
frame record probed readable first (the kernel copies it into a pipe, refusing with `EFAULT`
where it is unmapped). The task that catches a panic reads where it was raised from the same
ring, and each flush of the history writes the rows new since the last.

A fatal signal is recorded by an async-signal-safe handler (`blaze_profile::crash`), on the
alternate signal stack, into `<history>/crash.<pid>`: one page, mapped shared, of a fixed layout
(magic `BLZCRSH`, layout version, the executable's BLAKE3 and load address, then the signal, its
code and address, the thread, the faulting PC and each return address up the frame pointers from
the signal's context, the open spans and the thread's last panic). Then the signal goes on to the
handler that was there (the line `serve::signals` writes to the log, then std's), and ends the
process as it would have. The next daemon to open the history turns the record of each process
that is gone into a row, its addresses named by this executable's symbols at the dead process's
load slide, and deletes the file once the row is written. A record of another layout or another
blaze executable is refused with an error naming it and why, and set aside as
`crash.<pid>.refused`: another binary's symbols would name its addresses wrongly. The CLI a dead
daemon hung up on reads the record in place and quotes it with its top frames; `blaze slow`
lists a build's panics, and the signal its daemon died of, with theirs.

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
into the next flush, and counts what was still written over in the `profile rows skipped` counter.
Until the evening of 2026-11-02 the drain ran on the history's own thread every 250 ms and read
nothing while that thread flushed, and a flush of a busy build took 2 to 10 s: a self-build's
102 s build step skipped about 42 million rows. Now a thread of its own (`blaze-drain`) reads the
rings every `history::DRAIN` (50 ms), whatever the flush is doing, and asks for a flush once it
holds `history::FLUSH_ROWS` rows; past `history::BACKLOG_ROWS` it stops reading and counts what is
written over as `profile rows skipped behind`. Spans under `flight::FOLD_BELOW` (20 us) that no
row names are summed into `folded` rows (below) instead of a row each; on that self-build 64% of
the spans were under 2 us, and `find_container` and `Semantics::analyze_impl` (about 3 us) came
360,000 a second on a worker, more than a ring holds between two reads. The gate counts the folded roots' time as covered. It also fails on every sample of a worker blocked (`cpu = off`) while no wait span is open on its thread (`raw_blocks`, which names
the stack), and on every `wait` span without `awaits`. A check that saw nothing to judge
could not run, never passed, and the verdict does not pass: no pool worker switched in the build
(`workers_covered`), the build has no samples or none off the CPU (`blocks_in_waits`: a build
profiled with the sampler off would otherwise pass it by having no samples), or no `wait` span
(`waits_say_what`). Until 2026-11-04 `raw_blocks` matched macOS's symbol names (`psynch_cvwait`,
`kevent`, `read+0x...`), which matched nothing on Linux and nothing once frames lost their
offsets. Its other checks, and how CI runs it, are in "The gate" below.

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
- the daemon loop's `loop turn`, opened only for a turn with completions or ready work, and
  between turns its `wait` on the driver, whose `awaits` names what ends it: a plane task's
  end (the loop reads them every 1 ms while tasks run), a client's connection or request, a
  file, a child or a watch, the 50 ms idle tick, or the session's, store's or live feed's
  deadline.

- a build script's run: `build script run`, a root span open from the run's start on the daemon's
  loop to its end (its program's whole time, which no daemon thread spends), with the package
  and version in `unit` (`anyhow v1.0.98 (build script run)`). It is held (the field `held`): it
  is entered only while the loop does a step of the run, so its `self_cpu_ns` is the daemon's
  work for that run alone, and a run whose wall is far above it was its program's time.
- blaze's own toolchain downloaded into a home that lacks it: `toolchain download`, with the
  release in `toolchain` and the bytes in `bytes`. The client downloads it before the daemon
  starts; its span is in the client's flight recorder and reaches the history with the client's
  rows of the build it then asks for.
- the dependencies' image keys: `dependency image keys`, with the crates asked in `crates` and
  the database's view in `function` (`View(1234)`). A serve at the view the same crates were last keyed at, none
  moved, walks nothing and opens no span.

## Locks: who waited for whom

Every lock in blaze is a named `blaze_core::lock::{Lock, ReadLock, Signal}`
(docs/v2/plan/design/named-locks.md). An acquire that has to wait opens the same `wait` span the
scheduler's waits do (`awaits` = `lock <name>`, or `signal <name>` for a `Signal`), and once it has
the lock records one event of the target `blaze::lock`, message `lock_wait`
(`blaze_profile::locks::record`, the profiler being the locks' probe from `blaze_profile::install`
on). An uncontended acquire records nothing. Two views put the events' fields back as columns:

- `lock_waits`, one row a wait: `lock` (its name), `value_type` (the type it guards), `mode`
  (`lock`, `read`, `write`), `site` (the waiter's `file:line`), `span` and `thread` (the waiter's
  `spans.span_id`, 0 for none, and its OS thread), `start_ns` (on the profile's clock) and
  `wait_ns`; and the holder it waited behind, as the holder left itself at its release:
  `holder_site`, `holder_span`, `holder_thread`, `holder_mode`, `hold_ns` and `hold_whole` (the
  hold is from the holder's acquire when it too had waited; from when the first current waiter
  began when it took the lock uncontended, which has no clock read). A wait whose holder said
  nothing has `holder_thread` 0 and `holder_site` `''`.
- `stalls`, one row a stall the daemon's watchdog found in the wait-for graph (events of the
  target `blaze::stall`): `kind` (`deadlock` or `convoy`), `line`, `head` (the thread a convoy
  waits behind), `waiters`, `threads` (a comma list) and `oldest_ns`.

Thread ids are the profile's (the OS thread id, which the flight recorder gives the wait-for
graph as each thread claims its ring), so they join `spans.thread`.

`blaze debug locks [--build N] [--top N] [--json]` ranks a build's locks (the last that ended
unless `--build` says; 20 unless `--top` says) by total wait: each with its count, its share of
the pool workers' time, its three most waited call sites, the span its waiters waited in most and
the span its holders held it in most, and what to use instead, read from its rows by the first
rule that holds (`blaze_profile::locks::recommend`):

| when | instead |
| --- | --- |
| reads are 80% or more of the waits, and most holders that said themselves wrote | arc-swap or RCU |
| `value_type` is a `HashMap`, `BTreeMap`, `HashSet` or `IndexMap` | `papaya::HashMap` (shared), or `blaze_hash::HashMap` behind one owner |
| `value_type` is an integer or `bool` | an atomic |
| `value_type` is a `VecDeque` or `Vec` waited for on two threads or more | a crossbeam channel |
| every holder that said itself is one thread | move it into its owner and send it messages |
| otherwise | shorten the critical section |

The build's stalls follow the table.

```sql
-- the locks of build 12 by total wait, with their worst site
SELECT lock, count(*) AS waits, sum(wait_ns) / 1e6 AS wait_ms, arg_max(site, wait_ns) AS worst_site
FROM lock_waits WHERE build_id = 12 GROUP BY lock ORDER BY wait_ms DESC;
```

## The gate: what a profile must hold

`blaze debug gate [--build N] [--history DIR] [--json] [--summary FILE]` runs the gate
(crates/blaze-profile/src/gate.rs) over a build of the history: the last that ended unless
`--build` says, of the daemon's history unless `--history` names a directory. Each check passes,
fails, or could not run, and says which with a line and its offenders. A check that could not run
(its rows are missing, or its query failed) never passes: the verdict passes only when every
check passed, and its last line lists every check it could not run. Each check has one
threshold, a constant in gate.rs:

| check | holds | bound |
| --- | --- | --- |
| `workers_covered` | each pool worker's share of the build inside no span and not idle (`uncovered_per_worker`) | under `UNCOVERED_MAX_PCT`, 1% |
| `blocks_in_waits` | no sample of a worker blocked in the kernel outside a wait span (`raw_blocks`); could not run when the build has no samples or none off the CPU | none |
| `waits_say_what` | every `wait` span has `awaits` | none |
| `rows_lost` | the recorder's `profile spans lost` and `profile rows skipped` over the build | `ROWS_LOST_MAX`, 0 |
| `samples_in_spans` | the share of the workers' samples whose `span_id` names a span open as they were taken | at least `SAMPLES_IN_SPANS_MIN_PCT`, 99% |
| `children_in_parents` | every span with a parent row lies inside it | `CLOCK_SLACK_NS`, 10 us |
| `self_cpu_in_duration` | `self_cpu_ns` of every span and folded sum is 0 or more and no more than its duration | `CLOCK_SLACK_NS` a span |
| `cpu_matches_process` | the build's spans' and folded sums' `self_cpu_ns` summed against the daemon's own CPU over the build (`builds.daemon_user_ns` + `daemon_system_ns`) | within `CPU_DIFF_MAX_PCT`, 5% |
| `slow_deterministic` | `blaze slow --json` of the build gives the same bytes twice | none |
| `merged_rows_match` | every merged file of the build holds the rows its merge read from its inputs' footers (`blaze.merged_rows`) | none |
| `merges_exact` | the build's files of each table merged again (`history::concat`) hold exactly their rows: by the footers, and as a multiset of DuckDB's row hashes | none |
| `lock_waits` | no one named lock's waits (`lock_waits`) sum to more than a share of the build's pool-worker time (its workers, as `workers_covered` finds them, times its wall time); a build with no lock waited for passes, since an uncontended lock records nothing | `LOCK_WAIT_MAX_PCT`, 2% (set from the 2026-11-05 `flight::intern` convoy and the 1% uncovered budget; re-picked from the first ten gate builds) |

The daemon's CPU is its `builds` row's `daemon_user_ns` and `daemon_system_ns`
(`getrusage(RUSAGE_SELF)` at the build's end less at its start); a row with none (Windows, a blaze
before them) leaves `cpu_matches_process` unable to run. A merged file's Parquet metadata says the rows its merge read under
`blaze.merged_rows` (`history::MERGED_ROWS`), so a merge that dropped or doubled a row group is
caught after its inputs are gone.

The verdict names the build's `tree` and `blaze`, the exact tree and binary measured, and a
history with no such build is an error naming it. `--json` prints it as one object (version 1;
each check's `name`, `outcome` of `passed`, `failed` or `unchecked`, `said` and `detail`). The
command exits non-zero unless every check passed.

CI runs it in blaze-platform.yaml's `profile-gate` job, over the history the build job's
self-build wrote (the files newer than the build's start, handed over as the `profile-<platform>`
artifact), with the blaze that build made (harness/profile_gate.py). That history is written by
the release blaze that builds blaze, so a check of rows only a newer blaze writes cannot run
until a release writes them: harness/profile-gate-known.toml lists each check known to fail or
not run, per platform, with an owner and a cause, and the job fails on any other outcome and on a
listed check that passes. The same job runs the gate over each fixture of
crates/blaze-profile/fixtures/gate/, a history of one build per check that the check must fail
(written by harness/profile_fixtures.py; `gate::tests::each_fixture_fails_its_own_check` runs
them with `BLAZE_DUCKDB` set). `slow_deterministic` and `merges_exact` test blaze's own code over
any history, so no file can make them fail; history.rs tests the merge's count.

`--summary FILE` writes the build's per-build summary, `build_summary` (views.sql: one row a span
name, with the build's commit, tree, blaze, its runs, median, total and self CPU), as Parquet.
The job uploads it, with the verdict, as `profile-summary-<platform>-<runner>-<commit>-<tree>-<blaze>`
(each cut to 12 characters) for 90 days. profile-nightly.yaml takes the newest summaries of main
on one runner class, one a commit, and runs `slower_between_commits` (over `build_summary`) with
the newest commit as `later` (harness/profile_trend.py); it fails naming each span whose median
grew 1.25 times and by 50 ms or more.

## What made a build slow: `blaze slow`

`blaze slow [--build N] [--top N] [--json]` reads one build's profile (the last build that
ended or whose daemon died, unless `--build` says otherwise) and ranks what made it slow by the wall time each fix
would save. A build of 30 s or more ends with a note pointing at it. `blaze debug time` stays
the raw table; `blaze slow` is the verdict. It is a top-level command beside `blaze why`: `why`
says what a build computed again, `slow` says where its time went. It is not `blaze profile`,
because cargo's `--profile` means something else.

A build whose daemon died (killed, as an out-of-memory kill does, crashed, or ended by its
stop's bound) has no end row; its report runs to the daemon's last row, says "ended by the
daemon's death" with the pid, and names what each thread was inside at the daemon's last reading
of its open spans: each span, what a wait awaited and the job that ran it (`build.died` in the
JSON, report version 3). A build's panics, and the fatal signal its daemon died of, are listed
after it, each with where it was raised, its thread, its message and the top frames of its stack
past the panic machinery's own (`build.panics`, report version 5).

Before it picks the build, `blaze slow` reads back the flight recorders of daemons that died
with rows not yet flushed (when no daemon writes the history; a daemon does it as it opens):
their last seconds become rows of the history, and each build they were running gets a
`crashed` row. Such a build's report runs to the last row read back, not to the last flush.

A build is its number: its rows are the rows tagged with it, and the rows of no build in its
`builds` row's window from the same process. When rows of one number come from more than one
process (daemons of a blaze before 2026-11-03, which could each number a build 1 in one home),
`blaze slow` and `blaze debug time` refuse the build, naming each process with its rows and
time, rather than read them as one build of all their workers.

Each finding has these fields:
- a kind;
- the fix, in words;
- `saves_ns`;
- how that saving was estimated;
- what it is about (a span name, a wait's class, a query);
- the module or file that owns it, when the spans say (`spans.target`, or a `crates/...rs:N`
  in the subject);
- its evidence, with numbers;
- for a finding about a span name, `stacks`: the three commonest stacks the build's samples
  found inside those spans (a sample counts for its span and each span it is inside), on the CPU
  (`on_cpu`, where it computed) and blocked (`off_cpu`, what it waited in), each with its
  samples; the text shows each stack's innermost 8 frames, innermost first. Left out of the JSON
  when there are none (no samples, or a finding about a query, a wait class or the workers).

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
| `reread` | a query one of whose runs held 2^20 reads or more on its query stack (`query_counts.reads_most`), or whose runs recorded 8 reads or more for each read their rows kept (`reads_recorded` over `reads_kept`, with 2^20 recorded or more): runs re-reading the same rows, as a loop over items re-reading inputs does | unknown |
| `missed` | the `misses` rows by step and cause, except `first` | unknown |
| `shed` | the daemon's `live bytes` coming into the build (the build before's last value, or this build's first) against the least during it: a fall of more than 1 GiB, or of more than half of 256 MiB or more. What it let go is computed again | unknown |

The estimates are not additive. Each says what fixing that one thing alone would save, so a
wait and the owner's serial work under it can both count the same stretch of the path.

What the profile could not see is listed after the findings, as blind spots, never skipped:
- critical-path time the record lost (`unrecorded` links, by reason and lane);
- job running time inside no span, against the 1% gate;
- worker running time with no span CPU counted for it, beside the daemon's own OS CPU over the
  build (`builds.daemon_user_ns` and `daemon_system_ns`, from getrusage), which the gate checks
  the spans' total against (`cpu_matches_process`); a build row from before those columns says
  it has none;
- waits with no wait span (rule 15), classed by their kind alone (`key wait with no wait span`)
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
- a critical path that stops short of the build's start;
- rows the recorder lost (`rows_lost`): spans it had no slot for, rows a ring wrote over between
  drains, and of those the rows skipped while the history's writer was behind, each counted.
- spans the model did not read (`spans_cut`, report version 5): a build of more spans than
  `time::load::SPAN_BUDGET` (8 million) is modelled in two passes (`time::load::modelled`). The
  first reads the spans at or past the least floor (0, then 100 us, 200 us, 500 us, 1 ms, ...
  1000 s, in steps of 1, 2 and 5) that keeps them within 6 million, and every unended one; a
  child is never longer than its parent, so no span read loses its parent. The root `job` spans
  and the `wait` spans under that floor are summed in DuckDB instead, by name, crate and subject
  (a job's function, a wait's `awaits`): count, time, longest and CPU, the 64 groups that took
  the most time and a total by name. A first model of the switches and those spans gives a
  critical path; the spans of the lanes it crosses (each job's runs on their threads, each
  thread lane's life) are read again under the floor, from the least floor that keeps them
  within 2 million, and the final model is built from both. The report says both floors, the
  spans and time left out, the summed groups, the lanes on the final path the second pass did
  not read, and the jobs whose root span was under the floor (not judged for running inside no
  span). A park with no wait span shorter than the floor is classed `<kind> wait under the read
  floor`, not as an unnamed wait. The spans by name and the workers' CPU count every span. The
  only refusal left is a build whose unended spans alone pass the bound, which takes a recorder
  that left millions of spans open.


Its header says the recorder's loss on a line of its own, even when it lost nothing
(`recorder: lost no rows; summed N spans under 20us into folded (S); cost C (X ns a span over N spans, R thread-CPU reads)`), and `--json` has it as
`summary.recorder`. A total in a report whose recorder lost rows is short by what they held.

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

How far that reading goes is bounded. On 2026-11-03 one 52-minute cold build (build 24 of
`s05b-home`) held 192 million spans, 130 million of them under 5 us. `blaze slow` read every one
into the model, and was killed past 19 GiB of its own after DuckDB had taken 22 GB for that one
query. Since 2026-11-04:
- the build's spans are counted by duration in DuckDB first, and only the floor's are read
  (above): 7.4 million of build 24's, at 10 ms;
- a build past the budget even at the 1 s floor is refused (`TimeError::TooManySpans`), naming
  its count, rather than read in part;
- DuckDB runs with `sql::THREADS` (8) threads and `sql::MEMORY_LIMIT` (4 GB), spilling into the
  query's scratch directory. Left alone it took every core and 80% of RAM;
- queries that don't wait on each other's answers go to one DuckDB (`sql::run_all`). Making the
  views reads the footer of every file of the history (17 CPU-seconds over a home of 6,000
  files), so each extra process paid that again: `blaze slow` went from 13 DuckDB processes to 5.

## The flight recorder

Every blaze process keeps a fixed ring per thread, holding that thread's recent spans, counter
values, switches and events as `spans`, `counters`, `switches`, `events` and `event_fields` rows. Recording allocates nothing and does no I/O. A
span's fields (`crate`, `function`, `unit`, `awaits`) are interned the first time each value
is seen, in generations of 16,384 names a shard: a full one becomes the previous and the one
before it is let go, so a daemon's thousandth build names its keys as its first did. (Until
2026-11-01 the names were capped at 262,144 a process and dropped past it: build 24 of the
round 31 daemon had no `function` on its wait spans and most query spans.)
Next to it, a sampler records each recording thread's stack 49 times a second, running or
blocked inside a span, with its span and build (`samples`, above). Frames are named only when
they are written out.

Each row is 64 bytes (8 words): close and start ticks, span id, parent, a word packing the kind,
callsite, depth, the ring that made the span and its allocation count, the interned crate and
function, the build and unit, and the bytes allocated. A ring holds 32,768 rows (2 MiB). A span's
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

### Past a crash

The rows the history had not flushed die with the process unless they are somewhere the
kernel keeps. So the daemon's history persists the recorder (`History::persist_recorder`, as it
opens) into `flight-<pid>.map` in the history's directory, a file the daemon maps shared and
holds an exclusive lock on (`flight/postmortem.rs`):

- a header: magic, a byte-order mark, the layout's version, a fingerprint of every constant a
  row's decoding rests on (row bits, plot bytes, the plots, switches, waits and span columns by
  number), the pid, the clock's base (a tick, the monotonic clock read with it and the
  nanoseconds a tick lasts, in two slots so a torn write leaves the last whole one), the time of
  the last flush, the blaze hash and the process's name;
- every callsite's name, target and level, written as it registers;
- the builds running, written as each starts and freed as it ends;
- a log of the names rows hold, one a shard and one of threads' names, appended as a name is
  first given and read newest first;
- each ring's store: its head, the row the history flushed to, its last 64 owners (the tid and
  name of each thread that wrote it, from which row), and its 32,768 rows.

Recording is unchanged: a thread writes its own store with the same atomics, with no system
call and no allocation, and no row holds an address. Rings made before the history persisted
move into the file, each by its own thread as it next writes (a ring no thread holds moves at
once). The file is reserved whole as it is made: its fixed part (about 19.3 MiB) and a store
for each of the first 256 rings (`MAPPED_RINGS`; each 2 MiB of rows and a 4 KiB head, about
532 MiB in all), with `posix_fallocate` on Linux and `F_PREALLOCATE` on macOS, so a full disk
is an error at persist time naming the file and why, never a SIGBUS later. A ring past persist
takes its store with no system call. There is no heap fallback: a process with more than 256
rings as it persists refuses to persist (the error names the file), and after it persists a
thread past the 256th gets no ring (its spans count as ringless) and the next flush reports the
store as full. The map and the crash records (`crash.<pid>`) count against the history's cap
(4 GiB), so the files the history keeps plus them stay under it, the newest build excepted. On
Windows the history says it can't persist and records as before.

Each flush marks in every store the row it wrote to. A history that opens (and `blaze slow`
with no daemon) takes every `flight-*.map` whose lock no one holds, so of a process that died:
it refuses one of another layout or fingerprint with an error naming the file (the next flush
answers it; the file stays until a blaze that reads it does, or it is deleted), decodes the rows
past each store's flushed row with the decoder the live reads use, gives their names and sites
numbers of its own, writes them as files of the dead process, writes a `crashed` row for each
build it was running, and deletes the file. The blaze hash is kept but not checked: the file
describes itself (its layout's version and the fingerprint of every constant decoding rests
on, the event fields' too), so a blaze rebuilt after a crash, the usual case when the crash is
being fixed, reads it; a check of the hash would refuse exactly those files, and the
fingerprint already refuses every one this blaze would misread.

## The live feed

`blaze debug live` and the explorer's live stream each get a `live::Feed` of their own on the
daemon. Every 250 ms it takes what the rings got since its last frame, plus the `open` table,
and sends a frame: `at_ns`, `skipped`, `lost`, then the `spans`, `counters` and `open` tables.
Each table is one long-lived Arrow IPC stream per subscriber, compressed with zstd
(`live::Encoder`): the schema and each dictionary's values cross once, and each frame with rows
carries its batch and a delta of only the dictionary values it names first, so a span's name,
target, crate and thread cross once. A frame with no rows has no bytes for that table. When a
stream's dictionaries pass `live::Cap::LIVE` (65,536 values or 1 MiB a table), the next frame
with rows ends the stream with the IPC end-of-stream marker and starts a new one with a fresh
schema and dictionaries; the client's `live::Decoder` (one a table) follows it. Same rows in,
same bytes out. A subscriber that can't take a frame on time
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
| looks that found work running | a turn that finds a worker inside a job's code (`Pool::working`, one long compile), a run of user code alive, or a download that got a byte in the last second (`blaze_fetch::downloads`; a download that gets nothing stops itself after 30 s, naming its address) |

When none of them has moved for the stall bound, 120 s unless `blaze serve --stall-bound SECS`
says otherwise, the daemon:

1. writes `state/stalls/<time>.json`: the flight recorder's last two minutes and the CPU
   samples as a Chrome trace (ui.perfetto.dev opens it);
2. fails each running request with the error `blaze stalled: no job ended, no query
   completed, no task or step ended, no job's code or run of user code was running and no
   download got a byte for N s while M tasks ran (the stall bound is B s,
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

## `BLAZE_PROFILE`

`BLAZE_PROFILE=<dir> blaze build` exports the build's Chrome trace (ui.perfetto.dev) from the
history to `<dir>/trace.json`, the trace `blaze debug time --build N --trace FILE` writes: every
lane of the daemon's rows, the fibers' tracks and the critical path on a track of its own
(`time::load::trace_of`). The client makes it once its rows are written, and says its path on
stderr; a failure is a warning naming the step and the build, and the command's outcome stands.
The daemon needs no variable: the history is always written.

Removed on 2026-11-04, with every caller: the cross-process profiler `BLAZE_PROFILE` used to
turn on. Each process kept per-thread phase, function and pass costs while it was set, wrote
`<dir>/parts/<pid>-<n>.json` at exit (an `atexit` hook) or, in the daemon, at each build's end,
and the client put the parts together into `trace.json`, `phases.tsv`, `functions.tsv` and
`passes.tsv`. Gone with it: `BLAZE_STATS`'s step table (`blaze time: ...` and `blaze where the
time went`), `BLAZE_PROFILE_CONST` (one constant's requests and runs), and `blaze_profile`'s
`note`, `step`, `pass`, `write_part`, `discard`, `finish`, `table`, `breakdown`, `start`,
`from_env`, `child_env`, `on`, `dir`, `with_spans`/`split`, `link_output` and `Span`. Each was
a second record beside the history, written only when asked for, and the client's part was
the only record of the client. The history now has the client's rows, every span's own CPU,
each child's rusage and each test run, so a phase table is a query:

```sql
SELECT name, count(*) AS n, sum(dur_ns) / 1e6 AS wall_ms, sum(self_cpu_ns) / 1e6 AS cpu_ms
FROM spans WHERE build_id = 42 GROUP BY name ORDER BY cpu_ms DESC;
```

`harness/realbench.py` reads the exported trace's critical path in place of the old
`blaze.breakdown`; `crates/blaze-profile/src/diff.rs` still reads the `phases.tsv` of old
profile directories a log names. The flight recorder's dump beside a stall (`flight::dump`)
keeps its own one-process Chrome writer (`crates/blaze-profile/src/dump.rs`).

