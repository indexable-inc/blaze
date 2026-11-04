# Handoff: structured logs (lane pd-logs)

Worktree /Volumes/Projects/blaze-wt-pd-logs, branch pd-logs. Nothing is committed and nothing was built, checked or tested: the main agent runs `blaze check` and `blaze test` once.

## Files changed

- `Cargo.toml`: workspace `tracing` feature `release_max_level_info` changed to `release_max_level_debug`. Without this, release builds compile DEBUG out, so no DEBUG event could reach the ring. See risk 1.
- `crates/blaze-profile/src/logs.rs` (new). The single home for the event concept:
  - the keep rule: `keeps_event`, `keeps_event_named` (for the lint), `blaze_debug`;
  - the history policy: `Policy`, `POLICY_VAR = "BLAZE_HISTORY_LEVEL"`, `PolicyError`;
  - the sans-IO hold: `Hold`, with `HOLD_EVENTS = 1<<14` and `HOLD_BUILDS = 8`;
  - `Value`, a resolved field value with serde, by-bits Eq, Display and `json()`; and `fields_text`;
  - 4 tests.
- `crates/blaze-profile/src/flight.rs`:
  - new row byte `EVENT_FIELDS_ROW = 0xf8`, added to the row-byte assert;
  - `Ring::put` now returns the row's absolute index;
  - `event_id(ring, index) = ring<<48 | index`;
  - `FieldValue` (I64, U64, F64, Bool, Str(Sym)) and the `Logged` visitor (a fixed buffer of 32, replacing `Message`);
  - `evented` now returns `Option<Evented>`;
  - `EventRow.event_id`, new `FieldRow`, `Read.fields`;
  - decoding in `take()`, plus `window`, `clear` and `rows` updated;
  - `FIELDS_LOST` and `fields_lost()`.
- `crates/blaze-profile/src/schema.rs`:
  - `VERSION` 3 → 4, with a doc note;
  - `Type::Bool`;
  - `events.event_id` appended; the level and message docs changed;
  - new `EVENT_FIELDS` table, appended last to `TABLES`, making `[&Table; 24]`.
- `crates/blaze-profile/src/arrow.rs`: `Bool` maps to Arrow `Boolean`.
- `crates/blaze-profile/src/batches.rs`: an `event_id` column in `events()`, and a new `event_fields()` batch.
- `crates/blaze-profile/src/history.rs`. Localized edits:
  - a `hold` field and its init in `open`;
  - `Flushed` gains `event_fields`, `events_held` and `events_dropped`;
  - `build_ended` maps the outcome into `hold.ended`;
  - new `events_policy()` and `release_held()`;
  - one line in `flush` (`self.hold.sort(&mut self.read)` right after `drained.take`);
  - `write()` gets an `event_fields` batch and a 14th table entry;
  - `write_live` gets 5 tables;
  - the `dictionary()` arm for Bool.

  File discovery and crash recovery were not touched.
- `crates/blaze-profile/src/lib.rs`:
  - `pub mod logs`;
  - `Recorder::wanted`: events go through `logs::keeps_event`, spans keep their old rule;
  - `max_level_hint` is now DEBUG;
  - `Recorder::event` calls the new `logged()` when profiling;
  - new `Log` struct; `Recorded.logs`, `Part.logs` (serde default, skipped when empty);
  - `take`, `write_part` and `chrome()` emit the logs as instants through `time::trace::instant`.
- `crates/blaze-profile/src/time.rs`:
  - `EventIn` gains `build`, `event_id` and `fields: Vec<(String, logs::Value)>`, plus serde derives and `said()`;
  - `of_read` attaches `read.fields`.
- `crates/blaze-profile/src/time/load.rs`:
  - the events query adds `build_id` and `event_id`;
  - a 4th query reads `event_fields` (a SEMI JOIN on the build's events);
  - fields are attached by (process, event_id).
- `crates/blaze-profile/src/time/trace.rs`: `instant` is now `pub(crate)`. Its args carry typed fields, build, job, span and event_id; its name is message plus `key=value`.
- `crates/blaze-profile/src/time/html.rs`: the events table shows message plus fields.
- `crates/blaze-profile/src/time/tests.rs`: the `event()` constructor gets the new fields.
- `crates/blaze-profile/sql/views.sql`: new views `event_fields` (raw), `event_values` (adds `value_int` HUGEINT and `value_text`) and `events_wide` (`events` plus `fields` text plus fixed pivot columns `unit image path dir what why error outcome step bytes`).
- `crates/blaze-profile/sql/examples.sql`: `link_regions` and `link_code` now read typed fields through `event_values` and match `message = 'link region' / 'link code'`. The regex parsing of `message` is gone.
- `crates/blaze-profile/src/views.rs`: KEYWORDS gains `hugeint`; the doc lists the new views.
- `crates/blaze-lints/src/blaze/unrecorded_span.rs`:
  - `dropped_event` flags only an INFO-or-above event that `keeps_event` drops, so WARN and ERROR of any target are no longer flagged;
  - the test was updated: count 4; the WARN and ERROR events with no target or a foreign target are in the kept list.
- `crates/blaze-cargo/src/serve/observe.rs`:
  - `keep_history(state, duckdb, events: Policy)` calls `history.events_policy`;
  - the Disconnected path calls `history.release_held()` before the last flush.
- `crates/blaze-cargo/src/serve.rs`: reads `Policy::from_env()` once at start. An invalid value fails `blaze serve`, naming the variable.
- `crates/blaze-cargo/src/serve/unit/tests.rs`: the warn test asserts the message equals `MESSAGE` exactly, and that `event_fields` has `steps` with kind `i64` and `value_i64 = 3` under the same `event_id`.
- `harness/v2/worktree_keys.py`: `EVENTS_QUERY` reads `events_wide`, so the "unit record not kept" reasons still show unit and crate.
- `docs/profiling.md` (front matter kept):
  - schema version 4;
  - `events` and `event_fields` tables, the views, example SQL;
  - the keep table per level;
  - the history policy and `BLAZE_HISTORY_LEVEL`;
  - `BLAZE_PROFILE` parts carrying events.

## Design

- **Ring encoding.**
  - The event row is unchanged, except word 5 is now the message alone.
  - Each 5 fields add one `EVENT_FIELDS_ROW` with the values at `COL_WORDS` and the event_id at word 3.
  - Word 4 packs, per slot, `index<<3 | kind` in 9 bits from bit 8.
  - All fields are collected into `Logged` before any row is written, so an event logged from inside a field's `Debug` writes its own rows first and never interleaves.
  - Numbers store directly and never allocate. Text goes through `intern` (Debug values are formatted into the thread-local `TEXT`).
  - The reader matches field rows to `into.events.last()` by id, the same pattern span column rows use.
- **event_id.** The ring number (u16) << 48 | the absolute row index in that ring. It is unique per process for the process's life, and the reader recomputes it from `(ring.number, i)`. A parquet row is identified by (process, event_id).
- **Keep rule** (`logs::keeps_event`):
  - ERROR and WARN: any target.
  - INFO: `keeps` (a `blaze*` target or a `*_query` name).
  - DEBUG: target exactly `blaze` or `blaze::…` only. This is an explicit opt-in, so `blaze_fmt` and the like stay out of hot paths.
  - TRACE: never.

  Spans are unchanged (INFO and `keeps`).
- **History policy** (`logs::Hold`). Sans-IO, no clock, deterministic in the order of rows and build ends:
  - Under `info` (the default), DEBUG rows are held per build.
  - An ERROR event of the build, or `ended(Failed|Stalled)`, releases them into the next sort.
  - `ended(Ok|Cancelled)` drops them. DEBUG rows of build 0 are dropped.
  - `release_all()` runs when the history closes.
  - `debug` passes everything.
  - Caps: 16,384 held events per build (oldest evicted along with their fields) and 8 builds (lowest evicted). The remembered troubled and well sets keep at most 64 builds each.
  - Uses std `BTreeMap`/`BTreeSet` on purpose, for ordered and deterministic eviction. A reviewer may want blaze_hash plus explicit ordering instead.
- **Correlation.**
  - Every `events` row carries build_id, job, span_id and process, plus thread and event_id.
  - Under BLAZE_PROFILE, a process's part JSON now carries `logs` (an `EventIn` with build, job, span, process, event_id and typed fields). `trace.json` shows them as instants on the thread's track.
  - The stall dump already showed events and now includes their fields.

## Risks

1. **Perf (`release_max_level_debug`).** Release builds now compile in every `debug!`, `debug_span!` and `#[instrument(level="debug")]` callsite. The global max level is DEBUG, so each one is an atomic load plus a cached "never" interest check, about 1 to 2 ns. docs/v2/requests/db.md:11612 added `_info` to remove exactly that cost on solver and inference hot paths. Measure a self-build before and after (slow-is-p0-traced). Without lifting the cap, DEBUG events cannot exist in release.
2. **Row byte 0xf8.** Other lanes (crash signals, rings that survive a crash) may add a row byte too. The const assert catches a collision at compile time; pick another byte if so.
3. **Split reads.** A drain that reads an event row but not yet its field rows loses those fields: the next read can't match them to `events.last()`. This is rare, because the writer puts them back to back. The same limitation exists for span column rows.
4. **More sites registered.** WARN and ERROR events of every target now register flight sites (cap 2047). Only callsites that actually fire register, but a noisy dependency could use some up.
5. **Memory and crash.** Held DEBUG rows are daemon memory. The worst case is about 8 × 16k × (event plus fields), on the order of tens of MB. A daemon crash loses what was held, though the ring still has recent rows for the stall dump and `--live`.
6. **Old histories.** Schema version 3 files have no `event_id`: load.rs reads null as 0 with no fields. The writer rewrites `schema.<table>.parquet` on open, so `event_fields` and `event_id` always bind. The link_regions and link_code examples return nothing for v3 builds, which kept their fields in `message`.
7. **`events_wide` pivot columns** are a fixed list, chosen from a grep of the common keys. Anything else is in `event_values`.
8. **Part logs.** A log's `event.thread` is the OS tid, while its track `tid` is the profile's small tid. Each is consistent with its own table.

## Tests added or changed

- `logs::tests::an_events_typed_fields_are_rows_of_their_own_type_and_its_message_is_alone`: i64, u64, f64, bool, str and Debug fields round-trip through the ring into `event_fields` typed columns; ordinals hold; the message is alone; `event_id` matches between `events` and `event_fields`.
- `logs::tests::debug_rows_are_written_for_the_build_that_failed_and_dropped_for_the_one_that_ended_well`: opted-in DEBUG under two builds; a module-path DEBUG is not kept; `Hold` holds, then releases only the failed build's DEBUG row with its field; the ok build's rows are dropped; a release is written once.
- `logs::tests::the_history_level_is_info_or_debug_and_anything_else_is_named`.
- `logs::tests::warn_of_any_target_is_kept_and_debug_only_when_it_opts_in`.
- blaze-cargo `a_warn_with_no_target_written_in_blaze_cargo_reaches_the_events_table`: rewritten for message-only plus a typed field.
- blaze-lints `an_event_macro_the_recorder_drops_is_found_and_given_the_blaze_target`: WARN and ERROR events are kept.

## What to check

- `blaze check`, especially:
  - the flight.rs `Logged` visitor (`write!` on `RefMut<String>`, `Field::index`);
  - the thiserror format arg `var = POLICY_VAR` in logs.rs;
  - the let-chain in `Recorder::event`;
  - the `AsArray` `as_boolean_opt` and `Float64Type` imports in load.rs.
- `blaze test -p blaze-profile`: the logs tests, views `every_view_names_only_the_schemas_columns` (new SQL words: `hugeint`; aliases `said`, `value_int`, `value_text`), the history tests and the time tests. Also the blaze-cargo unit warn test and the blaze-lints unrecorded_span tests.
- `blaze explain link_regions` and `link_code` on a fresh build return rows (DuckDB `max(BOOLEAN)` and the `FILTER` pivot).
- Before and after a self-build for the tracing cap change (risk 1).
