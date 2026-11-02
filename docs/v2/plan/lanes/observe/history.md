---
status: plan
summary: lane observe, step 3: the flight recorder's Parquet history, its errors table and `blaze debug diff` (crates/blaze-profile/src/history.rs), the DuckDB views and named queries over it (sql/views.sql, sql/examples.sql, src/views.rs), `blaze debug sql` running the pinned DuckDB CLI (src/sql.rs), and the flush's speed of light.
date: 2026-10-31
---
# The history

The flight recorder is the one buffer; the history is the reader that keeps it. DuckDB and
DataFusion are never linked into blaze. blaze writes Parquet with the pure-Rust `parquet` crate,
and `blaze debug sql` runs the DuckDB CLI that `nix run .#duckdb` pins (nixpkgs' `duckdb` 1.5.5,
rev `d8cdaa33`, built with its in-tree extensions, parquet among them, so nothing is loaded or
downloaded at query time).

## Files

`History` (`crates/blaze-profile/src/history.rs`) owns a `flight::Cursor`, the directory, the
next flush's number and the buffers it reuses. `flush(build)` takes what is new in the rings
since the last flush, and the CPU sampler's new samples (named once a flush, through
`symbols.rs`), and writes one file a table that has rows (`spans`, `counters`, `builds`,
`samples`, `switches`):

    <dir>/<build>-<seq>.<table>.parquet

Here `build` is the build the flush is for: the daemon flushes once when a build request ends,
with its id. The timer's flushes (every 5 s while a session runs, `history::EVERY`) are named 0.
`seq` only grows within a directory; a reopened history continues from the highest one there.

**One file a table, not one file a flush.** A Parquet file has one schema, so a file holding
`spans`, `counters` and `builds` would need a union schema and a `table` column on every row.
With one file a table, each file describes itself (the Arrow schema with `blaze.table`,
`blaze.schema` and `blaze.clock` is in its footer), and each view reads one glob,
`*.<table>.parquet`.

**`schema.<table>.parquet`.** When a history opens, it writes an empty file of every table
beside the data files. With these, every glob matches at least one file, so a view over a table
nothing has written yet (`provenance` today) still binds, and `union_by_name` always sees
today's columns. An older file without a column that was added later reads it as null.

**Writing.** Each file is encoded in memory, into a buffer kept from file to file, then written
with one `write(2)` to `.<name>.tmp` and renamed into place, so a reader never sees half a
file. There is no fsync: a crash loses at most the last flush of a profile, and blaze's own
data never depends on it. `open` deletes the `.tmp` files a crash left behind.

**Encoding.** The files use zstd level 1, Parquet 2 data pages (delta-encoded integers), and
row groups of at most 122,880 rows, DuckDB's own row group size, which is the unit its scans
split across threads. Page statistics are on, so a query by `build_id` or time skips pages.
Dictionary pages are on for the strings and for the numbers that repeat (`build_id`,
`process`, `thread`, `depth`), and off for ids, instants and durations, whose dictionary would
be as large as the column.

**The commit.** `build_ended(BuildRow)` queues the build's `builds` row for the next flush.
The flush for that build gives the build's commit to the spans of that build only; spans of
other builds in the same flush get none. The views join `builds` for the commit whenever a
query needs it (`slower_between_commits` does).

**Where the files go, and how many.** The daemon's history is `profile/` in its state directory
(`history::dir_in(blaze_cargo::serve::state()?)`). It is capped at 1 GiB (`history::CAP_BYTES`):
after each flush, the oldest files are deleted until the rest fit under the cap. The newest
flush's files are always kept, and the schema files don't count toward the cap. At about 20 bytes a span, 1 GiB
holds about 50 million spans: days of a busy daemon.

**Nothing is skipped silently.** A write that fails is a `HistoryError` naming the file and the
step that failed (`writing`, `renaming into place`, `deleting the oldest file past the
history's cap`, a Parquet or Arrow error and its table). The rows of a failed flush are not
retried. Rows the rings wrote over before a flush could read them are counted in
`Flushed::skipped`. A ring holds 16,384 rows, so with 5 s flushes, a thread writing more than
about 3,000 rows a second loses some.

## Views

`crates/blaze-profile/sql/views.sql` is the prelude every query runs after. It defines one view
for each table (`spans`, `counters`, `builds`, `samples`, `provenance`, `inputs`,
`alloc_samples`, `heap_by_owner`), each over
`read_parquet([<globs>], union_by_name = true)`. It also defines these views over them:
- `queries`: the spans of queries, with `memo` (verified, reran, reran-same-output) and `cause`
  from `provenance`, matched on the build, the query's kind and its key;
- `passes`: the code generator's steps on one function (spans that name a `function` and are
  not queries), with the span's name as `pass`;
- `fiber_stacks`: one row per job that ran on a fiber, with its four `fiber stack` counters
  as columns and `high_water` (`deepest + grown`);
- `job_waits`: one row per wait of a job, its `parked` switch and the next `resumed` one, with
  the threads before and after, `waited_ns` and `moved`;
- `job_spans`: `spans` with the `job` running on the span's thread when it began (an ASOF join
  on `switches`), which the explorer's per-job lane draws from.

`views::prelude(&[dir, ...])` fills in each `@<table>` with the list of globs, one per
directory. That is the history's directory, plus the live window's directory with `--live`.

`sql/examples.sql` holds the named queries, each a `-- name:` line, a `-- says:` line, then its
SQL. `views::VIEWS` cuts them out at compile time, so a missing section is a compile error.
Their arguments are declared in Rust, each with a type and a default. The fourteen:
`slowest_queries`, `slower_between_commits`, `memo_hit_rate_per_crate`, `critical_path`,
`allocations_per_query_kind`, `what_reran`, `busiest_threads`, `longest_gaps`,
`top_cpu_stacks`, `time_by_span_name`, `frame_copies_per_query_kind`,
`jobs_by_stack_high_water`, `utilization_per_second` and `uncovered_time`. `views::render` binds the arguments as SQL literals:
- an integer as it is written;
- a float with an exponent, so DuckDB types it `DOUBLE`;
- text single-quoted, with each quote doubled.

An argument the view doesn't take, a missing one with no default, or one of the wrong JSON type
is a `ViewError` (`bad_args` on the explorer's wire). The explorer's `views` op sends each
view's arguments as JSON Schema (`views::args_schema`).

## `blaze debug sql`

`sql::run(&Query { duckdb, history, sql })` runs the following steps:
1. It checks that `duckdb` is an absolute path to a file. It is never looked up on `PATH`.
2. It writes a script: `SET autoinstall_known_extensions = false`, the prelude, then
   `COPY (<sql>) TO '<scratch>/answer.parquet' (FORMAT parquet, COMPRESSION zstd)`.
3. It runs `duckdb -batch -bail -init <empty file>` with the script on stdin. The empty init
   file keeps `~/.duckdbrc` out of it.
4. It reads the answer back with the `parquet` crate as Arrow `RecordBatch`es.

DuckDB's CLI has no Arrow IPC output without an extension, so the answer goes through a Parquet
file. If DuckDB refuses the query, the result is `SqlError::Duckdb` with DuckDB's own message,
the query and the script, and the scratch directory is kept so the script can be read.
`sql::run_live(query, window)` first writes the recorder's last `window` as
`live.<table>.parquet` (`history::write_live`), then queries it beside the history.

## Errors as rows

Each error a build says is a row of the `errors` table (schema.rs; `crates/blaze-profile/src/errors.rs`):
the build, the crate and the unit (`serde (build script)`), a stable `kind`, the function or
instance it is of, its first line, why (its first help or note, or a refused function's reason)
and its place. `kind` is rustc's code (`E0277`) or a deny lint's name when the error has one,
else blaze's: `derive-panic`, `attr-panic`, `macro-panic`, `expand-failed`,
`link-native-missing`, `link-failed`, `template-refs`, `refused`, `daemon-panic`, `uncoded`.
The error naming the functions blaze could not compile is a row a function, `template-refs`
when the template's references failed (its reason the innermost one), `refused` otherwise.
The daemon gives each error diagnostic to `errors::rows` and the rows to `History::errored`,
written by the build's flush. `errors::from_log` reads a build's printed log the same way, for
builds from before the table. Named queries: `derive_panics_per_build`, `errors_by_kind`.

## `blaze debug diff A B`

`crates/blaze-profile/src/diff.rs`. A side is a build of the history (its number, read through
DuckDB in one run for both) or a build log (a path: its errors by `from_log`, its phases from the
`phases.tsv` of the `BLAZE_PROFILE` directory the log names). It prints the phases (every span
of a name, whole wall time) and units (the time of a unit's five `unit ...` spans, by crate)
whose time changed, most first, at most 10 each, and the error kinds B has that A has not, and
those gone. What a side did not record it says, with why; a log has no unit's time.

## Speed of light: the flush

The flush runs off the workers, on the daemon's timer and at a build's end, so it is not on the
build's critical path. Its bound still matters: it decides how cheaply a busy daemon can keep
everything.

**Bytes a span.** In memory, an Arrow `spans` row is about 120 bytes: eight 8-byte columns,
about ten 4-byte dictionary keys, and a function name's offset and text. After Parquet's
encodings, the columns that are constant within a flush (`commit`, `process`, `process_name`,
mostly `build_id`) take a few bits a row, in runs. The dictionary keys (`name`, `target`,
`thread`, `thread_name`, `query_kind`, `crate`, `function`) take 1 to 2 bytes. The ids and
instants are delta-encoded, at 2 to 4 bytes each over `span_id`, `parent`, `start_ns`, `dur_ns`,
`alloc_bytes` and `allocs`. That is about 25 bytes before zstd, and about 20 bytes after it.

**The bound, per span**, on one core of an M-series or a recent x86 core:

| step | work | bound |
|---|---|---|
| read the ring | 64 bytes copied | ~1 ns (memory bandwidth) |
| encode the columns | ~20 values, delta, bit-packing, dictionary keys copied | ~20 ns |
| zstd level 1 | ~25 bytes at ~1 GB/s | ~25 ns |
| write | one `write` a file, ~20 bytes a span at > 2 GB/s | < 1 ns |

That is about 50 ns a span, or 20 million spans a second on one core. Writing 10,000 spans a
second, a busy daemon's rate, costs 0.05% of a core at the bound. The syscalls are one `open`,
`write`, `close` and `rename` per file, with at most three files a flush.

**Where this design stands, and what stands in the way.** The estimate from the code is 200 to
250 ns a span, four to five times the bound, so this is a known bug against the 10% rule. Three
costs make up the gap:
1. `batches::spans` interns every dictionary column's value per row through an `FxHashMap`
   (lane observe's code): about 25 ns.
2. arrow-rs's Parquet writer interns each dictionary value again, a hash per row per dictionary
   column (`ByteArrayEncoder`'s `Interner`, `parquet-60` `arrow_writer/byte_array.rs`). It does
   not reuse the keys of the Arrow dictionary it is given: about 100 ns across 7 columns.
3. The general column writer's per-value dispatch and level bookkeeping.

The fix for the first two: the ring already holds every name as a `Sym` or a site, so a writer
that turns them straight into a Parquet dictionary page and bit-packed keys hashes nothing per
row. That writer is blaze's own Parquet column encoder for the schema's few types (fixed-width
integers, dictionary strings and lists of them), and it can write the same files that DuckDB
and arrow-rs read. arrow-rs stays the reader and the reference implementation in tests (both
must give the same rows). The state of the art for this is DuckDB's own Parquet writer and
Polars', which encode dictionaries from existing keys.

**How to measure it.** In release, run
`blaze test -p blaze-profile --release -- --ignored --nocapture what_a_flush_costs`. It writes
a million spans and prints the nanoseconds a span spends going to Arrow and going to Parquet,
and the bytes a span takes. Not measured yet: lanes don't build.

## Tests

- `history::tests`:
  - a flushed table of each kind reads back as the same rows and Arrow types, dictionaries
    included;
  - a flush writes the recorder's new spans and the builds that ended, with the build's commit;
  - a reopened history continues numbering from its files and deletes half-written ones;
  - past the cap, the oldest files go first;
  - the live window has a file of every table.
- `views::tests`:
  - every section of examples.sql is a view with a title;
  - every declared argument is used, and every `$arg` is declared;
  - `render` binds and quotes each type, and refuses unknown, missing and mistyped arguments;
  - the prelude lists each table's globs and refuses glob characters in a directory;
  - every view's SQL names only the schema's columns, the names it gives itself, and DuckDB
    words on a fixed list.
- `sql::tests`:
  - the script installs nothing and wraps one query in `COPY`;
  - a DuckDB that is not an absolute path is an error;
  - an `#[ignore]` test, with `BLAZE_DUCKDB` naming the pinned CLI, runs every view and
    `--live` over a flushed history, and checks DuckDB's own message on a bad query.
