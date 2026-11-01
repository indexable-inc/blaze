-- The DuckDB views over blaze's Parquet history (crates/blaze-profile/src/history.rs): one a
-- table of crates/blaze-profile/src/schema.rs, and `queries` and `passes` over `spans`.
-- `blaze debug sql` runs this before each query, with every `@<table>` replaced by the list of
-- globs of that table's files, `['<dir>/*.<table>.parquet', ...]`: the history's directory and,
-- with `--live`, the recorder's last seconds beside it. Each directory holds an empty
-- `schema.<table>.parquet` of every table, so each glob matches a file with every column.

CREATE OR REPLACE VIEW spans AS
SELECT * FROM read_parquet(@spans, union_by_name = true);

CREATE OR REPLACE VIEW counters AS
SELECT * FROM read_parquet(@counters, union_by_name = true);

CREATE OR REPLACE VIEW builds AS
SELECT * FROM read_parquet(@builds, union_by_name = true);

CREATE OR REPLACE VIEW samples AS
SELECT * FROM read_parquet(@samples, union_by_name = true);

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

CREATE OR REPLACE VIEW errors AS
SELECT * FROM read_parquet(@errors, union_by_name = true);

CREATE OR REPLACE VIEW links AS
SELECT * FROM read_parquet(@links, union_by_name = true);

CREATE OR REPLACE VIEW crate_deps AS
SELECT * FROM read_parquet(@crate_deps, union_by_name = true);

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
-- `memo` is verified, reran or reran-same-output, matched on the build, the query's kind and
-- its key (the span's `function` field); null where provenance has no row for it.
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
