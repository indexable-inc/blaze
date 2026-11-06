-- The DuckDB views over blaze's Parquet history (crates/blaze-profile/src/history.rs): one a
-- table of crates/blaze-profile/src/schema.rs, `queries` and `passes` over `spans`,
-- `span_sums` over `spans` and `folded`, and `lock_waits` and `stalls` over `events`.
-- `blaze debug sql` runs this before each query, with every `@<table>` replaced by the list of
-- that table's files the history's manifest lists (`history/manifest.rs`), all of them or
-- those that can hold one build's rows: `['<dir>/schema.<table>.parquet', '<dir>/<file>', ...]`.
-- The list leads with the table's empty `schema.<table>.parquet`, which has every column, so
-- each view resolves with no file of its own: the reader writes those a history older than the
-- table lacks (`history::ensure_schemas`). No glob: a glob read a merge's output beside its
-- inputs.

CREATE OR REPLACE VIEW spans AS
SELECT * FROM read_parquet(@spans, union_by_name = true);

CREATE OR REPLACE VIEW counters AS
SELECT * FROM read_parquet(@counters, union_by_name = true);

-- One row a build: a build has a row as it starts (outcome `running`, no `dur_ns`) and another
-- as it ends, with the same `started_ns`, and the view keeps the end's. A build with only its start's row never ended: it
-- runs, or its daemon died (`history::writes` tells which). A build whose daemon died with its flight recorder persisted
-- has an end row of outcome `crashed`, written by the postmortem (`flight::postmortem`), to the last row read back. Once
-- its client sent its rows the end's row is written again with `client_started_ns` and `client_ended_ns`, and the view
-- keeps that one.
CREATE OR REPLACE VIEW builds AS
SELECT * FROM read_parquet(@builds, union_by_name = true)
QUALIFY row_number() OVER (
    PARTITION BY build_id, started_ns ORDER BY dur_ns DESC NULLS LAST, client_ended_ns DESC NULLS LAST
) = 1;

-- One row a child process blaze spawned and reaped, with its own CPU, memory and faults.
CREATE OR REPLACE VIEW processes AS
SELECT * FROM read_parquet(@processes, union_by_name = true);

-- One row a test a build ran.
CREATE OR REPLACE VIEW test_runs AS
SELECT * FROM read_parquet(@test_runs, union_by_name = true);

CREATE OR REPLACE VIEW samples AS
SELECT * FROM read_parquet(@samples, union_by_name = true);

-- Why the query engine ran what it ran (schema 4 on): a row a run that was not a first one,
-- with its cause and, for a changed read, the read by name (`cause_key`, which names another
-- row's `query_key` where the read is a memo), and a row an untracked read by name. Counts of
-- every run and verify, first runs included, are in `query_counts`.
CREATE OR REPLACE VIEW provenance AS
SELECT * FROM read_parquet(@provenance, union_by_name = true);

CREATE OR REPLACE VIEW inputs AS
SELECT * FROM read_parquet(@inputs, union_by_name = true);

CREATE OR REPLACE VIEW alloc_samples AS
SELECT * FROM read_parquet(@alloc_samples, union_by_name = true);

CREATE OR REPLACE VIEW heap_by_owner AS
SELECT * FROM read_parquet(@heap_by_owner, union_by_name = true);

CREATE OR REPLACE VIEW switches AS
SELECT * FROM read_parquet(@switches, union_by_name = true);

CREATE OR REPLACE VIEW events AS
SELECT * FROM read_parquet(@events, union_by_name = true);

-- One row a field of a kept event other than its message, its value in the column `kind` names
-- (i64, u64, f64, bool or str); `process` and `event_id` name its event, as in `events`. A file
-- of schema version 4 or older has no rows here: its fields are in its events' `message`.
CREATE OR REPLACE VIEW event_fields AS
SELECT * FROM read_parquet(@event_fields, union_by_name = true);

-- `event_fields` with every value as text (`value_text`) and an integer one, i64 or u64, as one
-- HUGEINT (`value_int`): what a filter on a field reads without knowing its kind.
CREATE OR REPLACE VIEW event_values AS
SELECT
    build_id, process, event_id, ordinal, key, kind, value_i64, value_u64, value_f64, value_bool, value_str,
    coalesce(CAST(value_i64 AS HUGEINT), CAST(value_u64 AS HUGEINT)) AS value_int,
    coalesce(CAST(value_i64 AS VARCHAR), CAST(value_u64 AS VARCHAR), CAST(value_f64 AS VARCHAR),
        CAST(value_bool AS VARCHAR), value_str) AS value_text
FROM event_fields;

-- `events` with each one's fields beside it: `fields` all of them as `key=value` text in the
-- order the event recorded them (null for none), then the keys blaze's events say most as
-- columns of their own, as text (null where the event has no field of that key). Every other
-- key is in `event_values`.
CREATE OR REPLACE VIEW events_wide AS
SELECT
    events.*, said.fields, said.unit, said.image, said.path, said.dir, said.what, said.why,
    said.error, said.outcome, said.step, said.bytes
FROM events
LEFT JOIN (
    SELECT
        process, event_id,
        string_agg(key || '=' || coalesce(value_text, '?'), ' ' ORDER BY ordinal) AS fields,
        max(value_text) FILTER (WHERE key = 'unit') AS unit,
        max(value_text) FILTER (WHERE key = 'image') AS image,
        max(value_text) FILTER (WHERE key = 'path') AS path,
        max(value_text) FILTER (WHERE key = 'dir') AS dir,
        max(value_text) FILTER (WHERE key = 'what') AS what,
        max(value_text) FILTER (WHERE key = 'why') AS why,
        max(value_text) FILTER (WHERE key = 'error') AS error,
        max(value_text) FILTER (WHERE key = 'outcome') AS outcome,
        max(value_text) FILTER (WHERE key = 'step') AS step,
        max(value_int) FILTER (WHERE key = 'bytes') AS bytes
    FROM event_values
    GROUP BY process, event_id
) AS said
    ON said.process = events.process AND said.event_id = events.event_id;

-- One row a wait for a named lock (blaze_core::lock, docs/v2/plan/design/named-locks.md): each
-- event of the target `blaze::lock` (message `lock_wait`, blaze_profile::locks::record) with its
-- fields as typed columns. `lock` is the lock's name and `value_type` the type it guards; `mode`
-- lock, read or write; `site` the waiter's `file:line`; `span` the waiter's span (a
-- `spans.span_id` of the same process, 0 for none) and `thread` its thread; `start_ns` when the
-- wait began, on the profile's clock, and `wait_ns` how long it took. The holder it waited
-- behind, as the holder left itself at its release: `holder_site`, `holder_span`,
-- `holder_thread`, `holder_mode`, and `hold_ns` how long it held the lock, from its acquire when
-- `hold_whole`, else only from when the first of the waiters then began. A wait whose holder
-- said nothing has `holder_thread` 0 and `holder_site` ''. `blaze debug locks` ranks these and
-- the gate's `lock_waits` sums them a lock.
CREATE OR REPLACE VIEW lock_waits AS
SELECT
    events.build_id, events.process, events.t_ns, events.event_id,
    max(f.value_str) FILTER (WHERE f.key = 'lock') AS lock,
    max(f.value_str) FILTER (WHERE f.key = 'value_type') AS value_type,
    max(f.value_str) FILTER (WHERE f.key = 'mode') AS mode,
    max(f.value_str) FILTER (WHERE f.key = 'site') AS site,
    max(f.value_u64) FILTER (WHERE f.key = 'span') AS span,
    max(f.value_u64) FILTER (WHERE f.key = 'thread') AS thread,
    max(f.value_u64) FILTER (WHERE f.key = 'start_ns') AS start_ns,
    max(f.value_u64) FILTER (WHERE f.key = 'wait_ns') AS wait_ns,
    coalesce(max(f.value_str) FILTER (WHERE f.key = 'holder_site'), '') AS holder_site,
    max(f.value_u64) FILTER (WHERE f.key = 'holder_span') AS holder_span,
    max(f.value_u64) FILTER (WHERE f.key = 'holder_thread') AS holder_thread,
    coalesce(max(f.value_str) FILTER (WHERE f.key = 'holder_mode'), '') AS holder_mode,
    max(f.value_u64) FILTER (WHERE f.key = 'hold_ns') AS hold_ns,
    max(f.value_bool) FILTER (WHERE f.key = 'hold_whole') AS hold_whole
FROM events
JOIN event_fields AS f ON f.process = events.process AND f.event_id = events.event_id
WHERE events.target = 'blaze::lock'
GROUP BY events.build_id, events.process, events.t_ns, events.event_id;

-- One row a stall the daemon's watchdog found in the wait-for graph (blaze_core::lock::stall):
-- each event of the target `blaze::stall` with its fields as columns. `kind` is deadlock or
-- convoy; `line` the one line `blaze status` says; `head` the thread a convoy waits behind;
-- `waiters` how many wait in it; `threads` the threads in it, a comma list; `oldest_ns` how long
-- the oldest of them had waited.
CREATE OR REPLACE VIEW stalls AS
SELECT
    events.build_id, events.process, events.t_ns, events.event_id,
    max(f.value_str) FILTER (WHERE f.key = 'kind') AS kind,
    max(f.value_str) FILTER (WHERE f.key = 'line') AS line,
    max(f.value_u64) FILTER (WHERE f.key = 'head') AS head,
    max(f.value_u64) FILTER (WHERE f.key = 'waiters') AS waiters,
    max(f.value_str) FILTER (WHERE f.key = 'threads') AS threads,
    max(f.value_u64) FILTER (WHERE f.key = 'oldest_ns') AS oldest_ns
FROM events
JOIN event_fields AS f ON f.process = events.process AND f.event_id = events.event_id
WHERE events.target = 'blaze::stall'
GROUP BY events.build_id, events.process, events.t_ns, events.event_id;

CREATE OR REPLACE VIEW errors AS
SELECT * FROM read_parquet(@errors, union_by_name = true);

-- Each panic and fatal signal of a blaze process, with its stack (`kind` panic or signal).
CREATE OR REPLACE VIEW panics AS
SELECT * FROM read_parquet(@panics, union_by_name = true);

CREATE OR REPLACE VIEW links AS
SELECT * FROM read_parquet(@links, union_by_name = true);

CREATE OR REPLACE VIEW crate_deps AS
SELECT * FROM read_parquet(@crate_deps, union_by_name = true);

CREATE OR REPLACE VIEW query_counts AS
SELECT * FROM read_parquet(@query_counts, union_by_name = true);

-- The spans every thread was inside at each flush that wrote rows, `read_ns` the reading: the
-- last of a process is what a daemon that died was doing.
CREATE OR REPLACE VIEW open AS
SELECT * FROM read_parquet(@open, union_by_name = true);

-- What each build computed again instead of reusing, and why (`blaze why`).
CREATE OR REPLACE VIEW misses AS
SELECT * FROM read_parquet(@misses, union_by_name = true);

-- The parts of the key of each miss (schema 4 on): what two builds' keys, or two homes', are
-- diffed by (`key_part_diff`).
CREATE OR REPLACE VIEW key_parts AS
SELECT * FROM read_parquet(@key_parts, union_by_name = true);

-- Profiled runs of programs blaze built (`blaze perf record`, crates/blaze-profile/src/perf.rs).
CREATE OR REPLACE VIEW perf_runs AS
SELECT * FROM read_parquet(@perf_runs, union_by_name = true);

CREATE OR REPLACE VIEW perf_functions AS
SELECT * FROM read_parquet(@perf_functions, union_by_name = true);

-- Sums of short spans, each under 20 us (a wait under 1 us) and named by no row, kept as one
-- row a site, parent's site, thread and stretch instead of a row each (`flight::fold_below`):
-- `spans` holds the rest, and a parent's row says what its folded children took
-- (`folded_spans`, `folded_ns`).
CREATE OR REPLACE VIEW folded AS
SELECT * FROM read_parquet(@folded, union_by_name = true);

-- The code images samples' and panics' `stack_pcs` are in, one row an image a process (once it held an
-- address a flush wrote): which file, its build id and BLAKE3, and where it was loaded.
CREATE OR REPLACE VIEW images AS
SELECT * FROM read_parquet(@images, union_by_name = true);

-- Every span, as a row of `spans` (`runs` 1) or as a sum of `folded` (`runs` its spans): what a
-- count, total or longest by name, query kind or thread reads, so the folded ones are counted.
-- A folded query's kind is its name, as a row's is.
CREATE OR REPLACE VIEW span_sums AS
SELECT build_id, process, thread, thread_name, name, target, query_kind, depth = 0 AS root,
    CAST(1 AS UBIGINT) AS runs, dur_ns, dur_ns AS max_dur_ns, self_cpu_ns, alloc_bytes, allocs
FROM spans
UNION ALL BY NAME
SELECT build_id, process, thread, thread_name, name, target,
    CASE WHEN ends_with(name, '_query') THEN name END AS query_kind, root = 1 AS root,
    CAST(spans AS UBIGINT) AS runs, dur_ns, max_dur_ns, self_cpu_ns, alloc_bytes, allocs
FROM folded;

-- What crates/blaze-profile/src/time.rs derives from `switches` and `spans`, for each build
-- `blaze debug time` has read: a job's intervals end to end, each span's time by its job's
-- states, the critical path, and each core's running time per second. Empty for a build it
-- has not read.
CREATE OR REPLACE VIEW job_intervals AS
SELECT * FROM read_parquet(@job_intervals, union_by_name = true);

CREATE OR REPLACE VIEW span_time AS
SELECT * FROM read_parquet(@span_time, union_by_name = true);

CREATE OR REPLACE VIEW path_links AS
SELECT * FROM read_parquet(@path_links, union_by_name = true);

CREATE OR REPLACE VIEW core_busy AS
SELECT * FROM read_parquet(@core_busy, union_by_name = true);

-- One row a time a job on a fiber parked: its `parked` switch and the `resumed` one after it,
-- with the thread before and after; `resumed_ns` and `thread_after` are null while it still
-- waits. `moved` says it resumed on another thread. A `wait_kind` of moved is the scheduler
-- moving it to another core, not a wait.
CREATE OR REPLACE VIEW job_waits AS
SELECT
    build_id, process, job, fiber, wait_kind, wait_value, parked_ns, resumed_ns,
    resumed_ns - parked_ns AS waited_ns, thread_before, thread_after,
    thread_before <> thread_after AS moved
FROM (
    SELECT
        build_id, process, job, fiber, wait_kind, wait_value, event, t_ns AS parked_ns,
        thread AS thread_before,
        lead(t_ns) OVER (PARTITION BY process, job ORDER BY t_ns) AS resumed_ns,
        lead(thread) OVER (PARTITION BY process, job ORDER BY t_ns) AS thread_after
    FROM switches
)
WHERE event = 'parked';

-- `spans` with the fiber job each ran in: the job running on the span's thread when it began
-- (that thread's last `started` or `resumed` switch before it, unless a `parked` or `ended` one
-- came after); null for a span outside any job. The explorer's job lanes are these spans by
-- `job` in place of `thread`.
CREATE OR REPLACE VIEW job_spans AS
SELECT spans.*, runs.job AS job
FROM spans ASOF LEFT JOIN (
    SELECT process, thread, t_ns, CASE WHEN event = 'started' OR event = 'resumed' THEN job END AS job
    FROM switches
) AS runs
    ON spans.process = runs.process AND spans.thread = runs.thread AND spans.start_ns >= runs.t_ns;

-- One row a span of a query, with what the memo did with that query when provenance has it:
-- `memo` is the row's outcome (ran, untracked read) and `cause` its cause, matched on the
-- build, the query's kind and its key. A query span's `function` is a display name, not the
-- memo's `` `query`(<key>) ``, so the match finds no row today: read `provenance` itself.
CREATE OR REPLACE VIEW queries AS
SELECT spans.*, memo.outcome AS memo, memo.cause AS cause
FROM spans
LEFT JOIN (
    SELECT build_id, kind, query_key, any_value(outcome) AS outcome, any_value(cause) AS cause
    FROM provenance
    GROUP BY build_id, kind, query_key
) AS memo
    ON memo.build_id = spans.build_id
    AND memo.kind = spans.query_kind
    AND memo.query_key = spans.function
WHERE spans.query_kind IS NOT NULL;

-- One row a job that ran on a fiber, from the four counters `fiber_stack` writes at its end
-- (crates/blaze-profile/src/flight.rs), in bytes: what its fiber reserved, how deep the job went
-- on it, how much of it is committed, and the segments the job grew past it. `high_water` is
-- how deep the job went in all. A job's number is reused, so a job is its fiber, number and
-- end together.
CREATE OR REPLACE VIEW fiber_stacks AS
SELECT
    build_id, process, fiber, job, t_ns AS ended_ns,
    max(value) FILTER (WHERE name = 'fiber stack reserved') AS reserved,
    max(value) FILTER (WHERE name = 'fiber stack deepest') AS deepest,
    max(value) FILTER (WHERE name = 'fiber stack touched') AS touched,
    max(value) FILTER (WHERE name = 'fiber stack grown') AS grown,
    max(value) FILTER (WHERE name = 'fiber stack deepest')
        + max(value) FILTER (WHERE name = 'fiber stack grown') AS high_water
FROM counters
WHERE fiber IS NOT NULL
GROUP BY build_id, process, fiber, job, t_ns;

-- One row a step of the code generator on one function: a span that names its function and is
-- not a query.
CREATE OR REPLACE VIEW passes AS
SELECT
    build_id, commit, process, thread, crate, function, unit, name AS pass, start_ns, dur_ns,
    self_cpu_ns, alloc_bytes, allocs
FROM spans
WHERE function IS NOT NULL AND query_kind IS NULL;

-- One row a span name a build: the per-build summary CI keeps for each self-build (`blaze debug
-- gate --summary`, `harness/profile_gate.py`) and `slower_between_commits` compares across
-- builds. Its rows are `spans`' alone, so a name only `folded` holds is missing. The one place it
-- is defined: a stored summary written with the build keeps these names and columns.
CREATE OR REPLACE VIEW build_summary AS
SELECT
    spans.build_id AS build_id,
    any_value(builds.commit) AS commit,
    any_value(builds.tree) AS tree,
    any_value(builds.blaze) AS blaze,
    any_value(builds.started_unix_ns) AS started_unix_ns,
    any_value(builds.dur_ns) AS build_dur_ns,
    any_value(builds.outcome) AS outcome,
    spans.name AS name,
    count(*) AS runs,
    median(spans.dur_ns) AS median_ns,
    sum(spans.dur_ns) AS total_ns,
    sum(spans.self_cpu_ns) AS self_cpu_ns
FROM spans
JOIN builds ON builds.build_id = spans.build_id
GROUP BY spans.build_id, spans.name;
