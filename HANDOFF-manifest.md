# Handoff: profile history manifest, flush-before-read, scoped reads, whole-build cap

Worktree `/Volumes/Projects/blaze-wt-pd-manifest`, branch `pd-manifest`. Not committed. Nothing was
built, checked or run. Every claim below comes from reading the code, not from running it.

## Files changed

New: `crates/blaze-profile/src/history/manifest.rs`.

Changed:
- blaze-profile: `history.rs`, `views.rs`, `sql.rs`, `sql/views.sql`, `schema.rs`, `clock.rs`,
  `reading.rs`, `perf/rows.rs`, `time/load.rs`, `slow/load.rs`, `slow/tests.rs`, `gate.rs`,
  `diff.rs`
- blaze-api: `lib.rs`, `borrowed.rs`, `frames/identity.rs`, `session.rs`, `session/tests.rs`
- blaze-cargo: `cli.rs`, `cli/observe.rs`, `serve.rs`, `serve/daemon.rs`, `serve/observe.rs`
- other crates: `blaze-explore/src/ops.rs`, `blaze-ui/src/api.rs`
- elsewhere: `fuzz/targets/api_envelope.rs`, `harness/live_e2e.py`, `docs/profiling.md` (front
  matter kept), `docs/v2/plan/lanes/observe/history.md`

## Design

### 1. The manifest is the single commit point

`manifest.json` lists the live files of each table, as
`BTreeMap<table, BTreeMap<file, Entry>>`, plus `flushed`, the last committed flush.
- **Encoding:** serde_json pretty, with sorted maps and a trailing newline, so the bytes are
  deterministic. `deny_unknown_fields`, `FORMAT = 1`.
- **Commit:** the file is rewritten to a hidden name and renamed over the old one, under
  `manifest.lock` (`stdx::file_lock::Locked`). Waiting for the lock is bounded at 5 s and opens
  a span; past that it fails with `ManifestError::Locked`.
- **Flush:** writes its files, then lists all of them in one commit. If one file fails, the
  files already written are still listed, then the error is returned.
- **Merge:** writes its output, commits "output listed, inputs unlisted" in one step, then
  deletes the inputs. A reader never sees both.
- **Cap:** unlists, commits, then deletes.
- **Other processes' writers** (`blaze debug time`'s derived tables, `blaze perf record`'s
  runs): `history::write_listed` writes to hidden names, then, holding the lock, renames each
  file into place and lists it.
- **Open (`sweep`, under the lock):**
  - deletes half-written hidden files (pid unparseable, our own pid, or a dead pid);
  - deletes every listable file the manifest does not list (crash garbage);
  - refuses a listed file that is missing (`ManifestError::Missing`);
  - refuses unlisted Parquet of no known form (`HistoryError::Unclassified`);
  - keeps listed files of unknown tables (from a newer blaze) as they are.
- **Adoption:** a directory with no manifest gets one built from its files, once.
  - Each file is classified by name, and its entry is read from its footer.
  - Unclassified Parquet is refused with a typed error.
  - Files a merged file already supersedes are left out, then deleted.
  - Readers adopt in memory only. The first writer writes the manifest.
- **Merge rule kept:** only files this History wrote since it opened (`ours`) are merged.

### 2. Readers use explicit lists, never globs

- `views::prelude(dir, &Manifest, Scope)` turns each `@table` into
  `[schema.<t>.parquet, listed files...]`, so every view still resolves.
- `sql::run_in` reads the manifest once per query.
- `reading::last` picks `open` files from the manifest by their entry's latest instant. It no
  longer uses mtime.
- `perf::rows::runs` lists from the manifest. A missing directory still gives `Ok(empty)`.

### 3. Per-file entries (review item a)

`Entry` has these fields:
- `version`: the schema version;
- `rows`;
- `builds: Option<[u64; 2]>`;
- `t_ns: Option<[i64; 2]>`: taken over the instant columns
  `t_ns`, `start_ns`, `started_ns`, `from_ns`, `to_ns`, `read_ns`;
- `boot: Option<String>`;
- `pid: Option<u32>`.

Where entries come from:
- **Flushes:** computed from the batches (`Entry::of_batches`).
- **Merges:** the union of the input entries (`Entry::merged`). Boot and pid are kept only if
  all inputs agree.
- **Adoption, and the test helper `list_written`:** read from the footer (`Entry::of_footer`).
  - The version comes from the `blaze.schema` key-value, or else from the embedded Arrow
    schema. A footer with neither is `ManifestError::Unversioned`. DuckDB fixtures pass the
    version explicitly.
  - Ranges come from row-group statistics. When a range has no statistics it is widened to
    the full range, so the reader reads more files, never fewer.

The boot id is new: `clock::boot()` reads `/proc/sys/kernel/random/boot_id` on Linux and the
sysctl `kern.bootsessionuuid` on macOS. A `None` boot matches any boot.

### 4. Build-scoped reads (review item b): partly done

- **Done:**
  - `Scope::{All, Build(n)}`.
  - `Manifest::scoped` / `of_build`: a file is included if its builds range contains n, or if
    its instants overlap the build's window on the same boot. The window is computed from the
    files whose builds range is exactly [n, n], widened by `history::EVERY`.
  - `sql::Query` gained a `scope` field, and `sql::run_all` gained a `scope` argument. Every
    caller is ported and currently passes `Scope::All`.
  - `time::load::query_many_in(source, scope, ...)` exists.
- **Not done:** `blaze slow` still uses `Scope::All`. I tried `Scope::Build` and reverted it,
  because it would change answers:
  - `recorder_sql` takes the last counter before the build window (`t_ns < t0`);
  - `memory_sql` takes `live bytes` at the end of the previous build;
  - `time::load::pick` and `rows` read across builds.

  To port it:
  - (1) Teach `Scope::Build` to also include, for each (boot, table) among `counters` and
    `builds`, the newest file that ends before the window.
  - (2) Run `pick` over the `builds` table only.
  - (3) Then switch `slow::load::facts` and `time::load::rows` to `Scope::Build`.

  The ~17 CPU-second saving (the comment at `sql.rs:167`) only arrives after that.
- **The unflushed tail:** covered by flush-before-read (section 7).

### 5. Retention by whole builds (review item c): partly done

- **Done:** `trim` now deletes whole builds, oldest first.
  - A build's age is the flush number of its newest file.
  - The build of the newest flush is never deleted.
  - Build-0 files (idle flushes that hold no build's rows) are deleted one at a time.
  - Unlist, then delete. A delete that fails does not stop the others; the first error is
    returned at the end.
- **Not done:** the permanent per-build summary. I did not add it. Proposed design:
  - A new table `build_summaries` in `schema::TABLES`, with views.sql
    `@build_summaries` and a docs column table.
  - One file per build named `summary-<build>.build_summaries.parquet`. It is not a `Name`, so
    it is never in `kept` and never capped (the same precedent as `perf-<run>`). Add it to
    `manifest::listable`.
  - Long rows: `(build_id, kind, name, count, total_ns, value)`.
  - It would be written by the daemon after the end flush of `Some(build)`. That needs DuckDB in
    the daemon (`Profiling.duckdb`) to run `slow::load::run`'s critical path and findings over
    `Scope::Build`, so it depends on item (b) being finished.
  - Span sums, query counts and recorder loss could instead be computed in Rust from the
    build's merged files.

### 6. Schema versions (review item d)

- `schema::UPGRADES` lists the older versions a reader still reads.
  - Version 2 reads as 3 with no rewrite: `folded_ns` is absent, reads as null through the
    schema file, and every reader treats null as 0. **No upgrade SQL was needed for 2→3.**
  - Version 1 cannot be upgraded: jobs were keyed by slot + 1, and the generation was never
    recorded.
- `schema::readable(version)` returns a typed reason. `views::prelude` refuses with
  `ViewError::Version { file, version, why }`, naming the file.
- Writers never refuse; files with an old version are dropped by the cap in time.
- docs/profiling.md now says version 3, not 1.

### 7. SQL sees unflushed rows (task 2)

- `Request::Sql` / `Response::Queried` are replaced by `Request::Flush` /
  `Response::Flushed { seq }`, at the same variant indices 27 and 32. `SCHEMA` 35 → 36.
- **Daemon:** `Observe::flush` sends `ToHistory::Flush(to)`. The `blaze-history` thread
  flushes, using the new step `FlushingAsked`, and sends `Told::Flushed { to, seq }`.
  - The loop answers, or fails at `FLUSH_WAIT` (30 s) naming `Doing::said()`.
  - `wait()`, `gone()` and `stop()` include pending flushes.
  - If the history thread is gone, pending flushes fail.
- **CLI:** `history_source()` (used by `debug sql`, `explain`, `slow` and `debug time`) runs
  only when `history::writer(&dir)` names a live owner. It asks for a flush, then checks
  `manifest.flushed() >= seq`.
  - If the daemon can't be asked, or the manifest does not show the flush, it fails with an
    error. There is no silent fallback.
  - With no live writer, every row is already in a file.
- **No double count:** a flush moves rows from the rings to a file, and the manifest commit is
  atomic.
- **`--live` and `--last` removed:**
  - deleted `sql::run_live*`, `SqlError::Live`, `history::write_live`, `Observe::query`, the
    `ipc` helpers and the IPC part of `NO_EXPLORER`;
  - updated the usage lines, `harness/live_e2e.py`, the fuzz envelope, blaze-ui and the docs.
- **Explorer:** queries running inside the daemon (`blaze-explore/src/ops.rs`) do not flush
  first. They use `Scope::All` and see up to `EVERY` of lag.

## Tests added or changed

In `history.rs`:
- `the_manifest_round_trips_through_flushes_a_merge_and_a_reopen`: the manifest bytes are equal
  after reopen, the files on disk equal `manifest.all()`, the encoding is deterministic, and
  each build row is read once.
- `a_merge_committed_and_not_deleted_reads_each_row_once`: checked before the commit, after the
  commit with the inputs still on disk, and after the reopen deletes them.
- `a_history_with_no_manifest_is_adopted_from_its_files`
- `a_parquet_file_blaze_did_not_write_stops_the_open_naming_it`
- `past_the_cap_a_build_goes_whole`

Elsewhere:
- `views.rs` `the_prelude_lists_each_tables_files_from_the_manifest`: explicit lists,
  build-scope selection, and refusal of version 1. Replaces the glob test.
- `sql.rs`: the script test now uses a manifest. The ignored DuckDB tests call
  `manifest::list_written` after their DuckDB fixtures. The `run_live` check is replaced by a
  counters count plus a `Scope::Build(77)` query.
- `slow/tests.rs` `history_of`: calls `list_written` after its fixtures.
- `serve/observe.rs`:
  - new `a_readers_flush_is_written_then_told_with_its_number`;
  - new `a_flush_past_its_deadline_fails_naming_the_history_thread` (weak: it races the real
    thread, so it accepts either outcome);
  - the no-profiling test is renamed and now checks the flush refusal.
- `blaze-api` `session/tests.rs`: the `Request::Flush` refusal name.
- Removed: `the_live_window_is_a_file_of_every_table`.

## Uncertain, please check

1. **Footer API.** `Entry::of_footer` uses parquet 60 statistics: `Statistics::Int64` with
   `min_opt`/`max_opt`/`null_count_opt`, `parquet::arrow::parquet_to_arrow_schema`, and
   `ColumnPath::string()`. Check the method names compile.
2. **Edition 2024 captures.** `Manifest::files`, `entries`, `scoped` and `of_build` return
   `impl Iterator + 'a`, which may also capture the `table: &str` lifetime. Every caller passes
   `&'static` names, but `use<'a>` may be needed.
3. **History tests and shared rings.** The new tests assume a warm-up `flush(None)` drains
   rows from other tests in the process. The round trip asserts `compacted >= 1` and a 4-row
   merged builds file. Both could flake if concurrent tests push higher build ids into the
   shared recorder (the same risk `a_history_opened_again` already accepts).
4. **Deletes under a running query.** A merge or cap deleting a file during a running DuckDB
   query fails that query with DuckDB's error. This race existed before; it is now documented.
5. **Old daemon writing concurrently.** A pre-manifest daemon writing at the same time as a new
   reader: its unlisted files are invisible, and a new daemon's open deletes them. Restarting
   the daemon fixes it. One-time.
6. **`Entry::of_batches` cost.** It scans the build_id and instant columns of every flush: one
   pass over the rows, with no allocation. A span would confirm the cost.
7. **`trim` and other processes' files.** `trim` only knows files in `kept`. Derived tables or
   perf files that another process lists after open are not counted until the next open. This
   was true before too.
8. **Session refusal text.** The `session.rs` refusal message changed (it names "the profile
   history's flush"). Any golden test on that text needs updating.

## What the main agent should check

1. Run `blaze check` across the workspace: blaze-profile, blaze-api, blaze-cargo (native and
   seed), blaze-explore, blaze-ui, fuzz.
2. Run `blaze test -p blaze-profile` (history, views, sql, perf, slow), then `-p blaze-api`
   (session, identity/frames round trips with SCHEMA 36), then `-p blaze-cargo serve::observe`.
3. Run the ignored DuckDB tests with `BLAZE_DUCKDB` set: `sql::tests::duckdb_*`,
   `the_critical_path_*`, `a_worker_outside_*`, `slow::tests::*`.
4. End to end, with a daemon running:
   - `blaze debug sql "select count(*) from spans"` returns rows from the last few seconds, and
     a second run does not double-count;
   - `blaze slow` and `blaze debug time` work;
   - an existing home with no `manifest.json` adopts once, and the file then appears.
5. Run `harness/live_e2e.py`: it now runs plain `blaze debug sql --json`.
6. Decide whether to finish item (b) (port `slow` to `Scope::Build` as in section 4) and item (c)
   (the `build_summaries` table) in a follow-up lane.
