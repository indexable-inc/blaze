# Handoff: profile coverage (lane pd-coverage)

Worktree `/Volumes/Projects/blaze-wt-pd-coverage`, branch `pd-coverage`. Nothing is built,
checked, tested or committed. The main agent runs the first `blaze check` and `blaze test`.

## What it does

1. **Client rows.** The daemon sends the build id in `Worked::build_id`. At its request's end
   the client sends its own flight-recorder rows (spans, events, folded), each tagged with that
   build, as `Request::ClientRows`. It also sends its start and end on the mono clock. The
   daemon writes them as the client's process (`process_name = "blaze client"`), and the times
   become `builds.client_started_ns` / `client_ended_ns`.
2. **`processes` table.** There is one row per reaped child, filled from the rusage in
   blaze-core `process.rs`. It goes through a new `blaze_core::reaped` observer into
   `facts::reaped`, then `facts::take_processes`, then a history flush.
3. **`test_runs` table.** Rows come from the test unit's `finished()` (the queue's `Finished`
   events). They travel through `RunPuts.test_runs`, then `job.test_runs`, then `Ended.test_runs`,
   then `History::tests_ran`.
4. **`BLAZE_PROFILE` is now an export from the history.** It writes `<dir>/trace.json` through
   `time::load::trace_of`, the same model, critical path and `trace::json` writer that
   `blaze debug time --trace` uses. The old cross-process profiler in `blaze-profile/src/lib.rs`
   is deleted along with every caller.
5. **Daemon CPU per build.** `builds.daemon_user_ns` / `daemon_system_ns` hold the getrusage
   `RUSAGE_SELF` delta between the job's opening and its end. `blaze slow`'s CpuGap blind spot
   now quotes them.

## Files changed

blaze-profile:
- `src/schema.rs`:
  - `BUILDS` gets four nullable columns at the end.
  - New `PROCESSES` and `TEST_RUNS` tables, added to `TABLES` (now 25).
- `src/clock.rs`: adds `mono_of(Instant)`.
- `src/facts.rs`:
  - Adds `ProcessRow`, `TestOutcome` and `TestRunRow`.
  - Adds the `CHILDREN` queue.
  - Adds `observe_children`, `reaped`, `process_row`, `take_processes`, and the batch makers `processes` and `test_runs`.
- `src/history.rs`:
  - `BuildRow` gets `client_ns` and `daemon_cpu_ns`.
  - Adds `ClientRows`, `CLIENT_PROCESS`, `client_rows()`, `processes_reaped()` and `tests_ran()`.
  - Adds `pending_clients` and the `recent` row cache (64 rows).
  - Adds `HistoryError::Client`.
  - `write()` now writes 15 tables.
  - Two new tests.
- `sql/views.sql`:
  - The `builds` view's order prefers the row with `dur_ns`, then the one with client times.
  - Adds the `processes` and `test_runs` views.
- `src/slow.rs` and `src/slow/load.rs`: `BuildFacts` gets `daemon_user_ns` / `daemon_system_ns`, and the CpuGap evidence and fix text use them.
- `src/time/load.rs`:
  - `build_process` leaves out `process_name = 'blaze client'`.
  - New `trace_of(source, build)`.
- `src/lib.rs`: the old profiler is deleted (about 1,070 lines). It keeps:
  - `began`, `install` (without `from_env`), the fiber/pool hooks, `Built`/`built` and `thread_cpu`.
  - The subscriber (`Recorder`, `TARGET`, `LEVEL`, `keeps`, `keeps_level`).
  - `command()`, now `pub(crate)`.
  - `opened`/`closed`, simplified to the flight recorder's calls.
  - The scope-wait test and the two `#[ignore]` measurement tests.
  - The module doc is rewritten and says what was removed.
- `src/dump.rs` (new): `Part`, `Event`, `Counter`, `Sampled` and `chrome()`, moved out of lib.rs for `flight::dump`. Without step spans, the `blaze` key holds `start_us`, `total_us`, the folded samples and the sampler, and no longer has `breakdown`.
- `src/flight.rs`: the import now comes from `crate::dump`, the `crate::dump::chrome` call drops the `&[]` spans argument, and two doc lines changed. Three small hunks.
- `src/sampler.rs`: the import changes to `crate::dump::Sampled`.
- `src/span.rs`: `open_id` no longer calls the removed `profile_began`.
- `src/time/trace.rs`: two doc references now point to `crate::dump::chrome`.
- `Cargo.toml`: the description changed (mirrored in `tools/nix/units/daemon.json`).

blaze-core:
- `src/reaped.rs` (new):
  - `Label` and `Reaped`.
  - `label(tag, kind, unit)`, a bounded list of 4096 entries.
  - `observe(context, reaped)`, set once.
  - The crate-private functions `observed`, `take` and `report`.
- `src/lib.rs`: adds `pub mod reaped;`.
- `src/process.rs`:
  - `Finish` gets `spawned`, `sandbox` and `executed`.
  - `exited()` reads the Effect tag before the reap (only when observed), takes the label, and reports a `Reaped`.
  - Adds `cpu(timeval)`.

blaze-api:
- `src/lib.rs`:
  - `SCHEMA` is now 36.
  - Adds `Request::ClientRows { build, events, folded, spans, ended_ns, started_ns }`, in canonical field order.
  - Adds `Worked::build_id: Option<u64>`.
- `src/frames/identity.rs`: `REQUESTS` is now 35, with a ClientRows generator; the `Worked` generator fills `build_id`.
- `src/session.rs`: adds ClientRows to the loop-answered arm and to `request_name`.
- `src/session/tests.rs`: `Worked` literal updated.

blaze-cargo:
- `src/cli.rs`:
  - Adds `pub mod client_rows;`.
  - `drive()` calls `client_rows::heard(worked.build_id)`.
  - The two `blaze_profile::note/finish` blocks are replaced by `sent_client_rows(err, color)` after `client.close()`.
  - The comment at install is updated.
- `src/cli/client_rows.rs` (new):
  - `heard` and `send`. `send` does nothing when no build was heard and in the seed: the seed's engine runs in-process, and a second connect would start a second engine.
  - `rows`, `encoded` and `export` (writes `<dir>/trace.json`).
- `src/cli/live.rs`: `heard()` on `Work`.
- `src/cli/observe.rs`: adds `history_paths()`; `history_source` uses it.
- `src/cli/print/tests.rs`: `Worked` literals updated.
- `src/raw/process.rs`: adds `cpu_times(Whose)`; `cpu_time` uses it.
- `src/serve/job.rs`:
  - Removed: the old `profile_dir`, the `profiled` field, the `blaze_profile::start` setup and the `write_part`/`discard` block.
  - Adds `test_runs`, `opened_cpu`, the `daemon_cpu()` delta in `row()`, and `build_id` in `worked()`.
- `src/serve/job/tests.rs`: the `profile_dir` test is removed.
- `src/serve/work.rs`: adds `RunPuts.test_runs`.
- `src/serve/observe.rs`:
  - `Ended.test_runs`.
  - `ToHistory::Client` and `Told::ClientWritten`.
  - `client_rows()`, which answers Done after the write and flush.
  - `facts::observe_children()` once the history opens.
- `src/serve/daemon.rs`: ClientRows is routed to `observe.client_rows` and does not mark `changed_since_settle`.
- `src/serve/unit/test.rs`: adds `test_run_row`; `finished()` pushes rows.
- `src/serve/unit/script.rs`: `label_run` labels spawns "build_script".
- `src/serve/unit/run.rs`: labels spawns "run".
- `src/serve/settings.rs`: `BLAZE_PROFILE_CONST` is dropped from `STARTED_WITH` (its reader is gone).
- `src/toolchain/own.rs`: the `blaze_profile::note` step is removed (the span stays).
- `Cargo.toml`: comment updated.

blaze-usercode:
- `src/test/queue.rs`: two spawn sites labelled "test".
- `src/pool.rs`: labelled "warm_server".

blaze-ui:
- `src/api.rs`: `Worked` literals get `build_id: None`.

Harness and docs:
- `harness/realbench.py`: the breakdown is now read from the exported trace's "critical path" track, summed by link kind, plus "outside the critical path". The old `blaze.breakdown` key is gone.
- `harness/bench.py`: the dead `phases.tsv` branch of `thread_steps` is removed; nothing passed `profile`. The `csv` import is dropped.
- `docs/profiling.md`: the front matter is kept, with `summary` extended. It documents:
  - The builds columns and the client rows.
  - The `processes` and `test_runs` tables.
  - The CpuGap text.
  - The toolchain download span.
  - A new `BLAZE_PROFILE` section listing everything removed, with a phase-table SQL replacement.
- `docs/README.md`: summary line synced.

## Design notes

- Only the daemon writes the history. The client's rows reach it over the socket, never as files.
- `History::client_rows` re-wraps each IPC batch in `schema_of(table)` and refuses any row whose `build_id` is not the request's build (`HistoryError::Client` names the build and the table). It then flushes, and the loop answers the client's request on `Told::ClientWritten`, so the export afterwards sees the rows. The client waits up to about 50 ms (the idle tick).
- Client rows may arrive before the build's end row. The times are then held in `pending_clients` and put on the end row. Otherwise the recent row (from `recent`, 64 builds) is written again with client times. The `builds` view dedups by (build_id, started_ns) and prefers the row with `dur_ns`, then the one with client times.
- `blaze_core::reaped` is a hook: no observer means no label kept and no extra read at reap. Only the daemon with an open history installs it, so a client never builds up rows.
- Labels are keyed by the spawn effect's `Tag` and taken back at reap. The list is bounded at 4096; spawns that never started drop out oldest first.

## Risks

- Frame size. The client's rows go in one request frame. A client that recorded more than the frame limit (16 MiB) fails the send. That is a warning on stderr; the build's outcome stands. Rings are 32k rows a thread, so this is unlikely but possible for a long `blaze test` live view.
- Merge conflicts. history.rs, flight.rs, schema.rs and lib.rs are shared with other lanes:
  - flight.rs is three hunks.
  - lib.rs is a wholesale rewrite: the "cut recorder per-span cost" lane edits `opened`/`closed` there. Take this version's shape and re-apply their change to the two small functions left.
- `blaze debug time` and the client. `time/load.rs::build_process` leaves out the client by `process_name`. If another lane renames the `process_name` column or adds per-process selection, recheck that filter. The model never mixes client spans with the daemon's.
- Sans-IO purity. `reaped::label` reads a global observer from inside the spawning cores (script.rs, run.rs, test queue, pool). It is a side effect outside the decision outputs. The context function reads the flight recorder's current build and span on the calling thread, so a label made on a different thread from the build's gets build 0.
- Unlabelled spawns. Spawns from the threads driver's `run_process`, test fork-server children, and Windows `iocp` are not covered. They show as `unlabelled` or not at all.
- `BuildFacts` gained two fields. Anything that serializes it, such as `blaze slow --json` and the gate fixtures, now has two more keys.
- Exact schema match. `client_batches` checks that the IPC batch's columns match `schema_of(table)` by re-wrapping. A client and daemon from different blaze builds whose span schema differs get a typed refusal. SCHEMA 36 should already keep them apart.
- History compaction. In the round-trip tests it could rename files before `read_back`. The tests flush and read in the same build; check that they pass and are not flaky.
- seed. Client rows are never sent in the seed (see `client_rows::send`); its in-process engine's history already has the process.
- `diff.rs` still reads `phases.tsv` from old `BLAZE_PROFILE` directories named in logs. That is deliberate (old records), but no new run writes `phases.tsv`.
- `tools/nix/units/daemon.json`: the description was hand-edited to match `crates/blaze-profile/Cargo.toml`. If that file is generated, regenerate it instead.

## Tests added

In `crates/blaze-profile/src/history.rs`:
- `a_child_process_row_and_a_test_run_row_read_back_column_for_column`: flushes one `ProcessRow` (build 0, written as null) and one `TestRunRow`, reads them back from Parquet, and compares column by column.
- `a_clients_rows_are_written_with_its_builds_row_given_its_times`: checks:
  - The client's span lands under `process_name = "blaze client"`.
  - `client_started_ns`, `client_ended_ns`, `daemon_user_ns` and `daemon_system_ns` read back.
  - A row of another build is refused with `HistoryError::Client`.

Removed tests (their code is gone): `the_rows_add_up_to_the_whole`, `a_profile_is_put_together` (lib.rs), `a_job_is_profiled_when_its_command_line_names_a_directory` (job/tests.rs).

## What the main agent should check

1. `blaze check` over the workspace. Likely compile issues:
   - Unused imports in lib.rs and job.rs after the deletion.
   - `dump.rs`'s `pub(crate)` visibility as used from `sampler::taken`.
   - The `encoded<E: Display>` generic in `cli/client_rows.rs` (blaze-cargo has `arrow-array` but not `arrow-schema`).
   - `blaze_core::reaped` imports in usercode.
   - The `Finish` field renames in `process.rs`.
   - The `cfg!(feature = "native")` path in the seed profile.
2. `blaze test -p blaze-profile`: the two new history tests, `slow` tests (`BuildFacts` fields), the `schema`/views tests (25 tables, new views), and the `TABLES`-count asserts.
3. `blaze test -p blaze-api`: the frames identity test (REQUESTS 35, generator index of `ClientRows`) and the schema-version tests.
4. `blaze test -p blaze-cargo`:
   - The settings test (`STARTED_WITH` length changed).
   - The serve/observe tests (updated `Ended` literals).
   - The print tests.
5. End to end on the Mac: `BLAZE_PROFILE=/tmp/p blaze build` in a small project. Then check:
   - `/tmp/p/trace.json` opens in ui.perfetto.dev.
   - `blaze debug sql "select * from processes"` lists build-script runs with rusage.
   - `select * from test_runs` after `blaze test`.
   - `select build_id, client_started_ns, client_ended_ns, daemon_user_ns from builds`.
   - `blaze slow` shows the daemon CPU in the CpuGap evidence.
6. Grep for any remaining `blaze_profile::{note,finish,step,pass,on,dir,start,child_env,VAR}`: none should be left, but a lane that merged in between may have added one.
