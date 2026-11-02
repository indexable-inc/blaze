-- Named queries over the views of views.sql: what the explorer's `views` op lists and
-- `blaze explain <view> <args>` runs. Each starts at a `-- name:` line, then a `-- says:` line,
-- then its SQL. `$<arg>` is an argument, declared with its type and default in
-- crates/blaze-profile/src/views.rs and bound there as an SQL literal. `$build` is the latest
-- build when not given.

-- name: slowest_queries
-- says: the slowest query runs of a build, with what the memo did and why
SELECT query_kind, crate, function, dur_ns / 1e6 AS ms, alloc_bytes, memo, cause
FROM queries
WHERE build_id = $build
ORDER BY dur_ns DESC
LIMIT $limit

-- name: slower_between_commits
-- says: the spans whose median time grew most from the commit `earlier` to the commit `later` (prefixes of either)
WITH runs AS (
    SELECT builds.commit AS commit, spans.name AS name, spans.dur_ns AS dur_ns
    FROM spans
    JOIN builds ON builds.build_id = spans.build_id
), earlier AS (
    SELECT name, median(dur_ns) AS dur_ns, count(*) AS runs
    FROM runs
    WHERE starts_with(commit, $earlier)
    GROUP BY name
), later AS (
    SELECT name, median(dur_ns) AS dur_ns, count(*) AS runs
    FROM runs
    WHERE starts_with(commit, $later)
    GROUP BY name
)
SELECT
    later.name AS name,
    earlier.dur_ns / 1e6 AS earlier_ms,
    later.dur_ns / 1e6 AS later_ms,
    (later.dur_ns - earlier.dur_ns) / 1e6 AS slower_ms,
    earlier.runs AS earlier_runs,
    later.runs AS later_runs
FROM later
JOIN earlier ON earlier.name = later.name
ORDER BY slower_ms DESC
LIMIT $limit

-- name: memo_hit_rate_per_crate
-- says: of the queries a build verified or reran, per crate, how many the memo kept (verified, or reran to the same output)
SELECT
    crate,
    count(*) FILTER (WHERE outcome = 'verified') AS verified,
    count(*) FILTER (WHERE outcome = 'reran-same-output') AS same_output,
    count(*) FILTER (WHERE outcome = 'reran') AS reran,
    round(count(*) FILTER (WHERE outcome <> 'reran') / count(*), 3) AS hit_rate
FROM provenance
WHERE build_id = $build
GROUP BY crate
ORDER BY reran DESC

-- name: critical_path
-- says: a build's critical path, first step first: from the work that ended the build back through each wait to whoever ran what it waited on (a wait's owner), each step what ran on which lane (a fiber job, or a thread) and for how long, a wait nobody owns a step of its own; on every row the whole path's seconds, its share of the wall time (near 100: the path bounds the build) the build's parallelism (busy threads on average), and whether the walk reached the build's start within its bound of 100000 steps (false: the path is cut short)
WITH RECURSIVE bounds AS (
    SELECT started_ns AS t0, started_ns + dur_ns AS t1
    FROM builds
    WHERE build_id = $build
), laned AS (
    SELECT process, span_id, name, crate, function, awaits, owner, depth, start_ns, start_ns + dur_ns AS end_ns,
        coalesce(job, CAST(thread AS UBIGINT) + 4611686018427387904) AS lane
    FROM job_spans
    WHERE build_id = $build
), waits AS (
    SELECT process, span_id, lane, awaits, owner, start_ns, end_ns FROM laned WHERE name = 'wait'
), job_starts AS (
    SELECT process, job, job & 4294967295 AS slot, t_ns
    FROM switches, bounds
    WHERE event = 'started' AND t_ns <= t1
), job_ends AS (
    SELECT process, job, min(t_ns) AS ended_ns FROM switches WHERE event = 'ended' GROUP BY process, job
), matched AS (
    SELECT waits.*, job_starts.job AS slot_job
    FROM waits ASOF LEFT JOIN job_starts
        ON job_starts.process = waits.process AND job_starts.slot = waits.owner - 1 AND job_starts.t_ns <= waits.end_ns
), owned AS (
    -- A slot's last job to start is the owner only if it had not ended when the wait began: a
    -- runner on no fiber (a client thread's job) has no switches, and its slot's older job is not it.
    SELECT matched.*,
        CASE WHEN job_ends.ended_ns IS NULL OR job_ends.ended_ns >= matched.start_ns THEN matched.slot_job END AS owner_lane
    FROM matched LEFT JOIN job_ends ON job_ends.process = matched.process AND job_ends.job = matched.slot_job
), stepped AS (
    SELECT owned.*,
        CASE WHEN owner_lane IS NOT NULL AND owner_lane <> lane THEN owner_lane ELSE lane END AS next_lane,
        CASE WHEN owner_lane IS NOT NULL AND owner_lane <> lane THEN end_ns ELSE start_ns END AS next_t
    FROM owned
), hops AS (
    SELECT stepped.*, earlier.span_id AS next_wait
    FROM stepped ASOF LEFT JOIN (SELECT process, lane, span_id, end_ns FROM waits) AS earlier
        ON earlier.process = stepped.process AND earlier.lane = stepped.next_lane AND earlier.end_ns < stepped.next_t
), last_span AS (
    SELECT laned.process, laned.lane, least(laned.end_ns, t1) AS t
    FROM laned, bounds
    WHERE laned.depth = 0 AND laned.start_ns < t1
    ORDER BY laned.end_ns DESC
    LIMIT 1
), start AS (
    SELECT last_span.process, last_span.lane, last_span.t, earlier.span_id AS wait
    FROM last_span ASOF LEFT JOIN (SELECT process, lane, span_id, end_ns FROM waits) AS earlier
        ON earlier.process = last_span.process AND earlier.lane = last_span.lane AND earlier.end_ns <= last_span.t
), chain AS (
    SELECT 0 AS step, process, lane, t, wait FROM start
    UNION ALL
    SELECT chain.step + 1, hops.process, hops.next_lane, hops.next_t, hops.next_wait
    FROM chain JOIN hops ON hops.process = chain.process AND hops.span_id = chain.wait
    WHERE chain.step < 100000
), lane_starts AS (
    SELECT process, lane, min(start_ns) AS start_ns FROM laned GROUP BY process, lane
), runs AS (
    SELECT chain.step, chain.process, chain.lane, 'ran' AS kind,
        coalesce(waits.end_ns, greatest(lane_starts.start_ns, (SELECT t0 FROM bounds))) AS from_ns, chain.t AS to_ns,
        CAST(NULL AS VARCHAR) AS awaits
    FROM chain
    LEFT JOIN waits ON waits.process = chain.process AND waits.span_id = chain.wait
    LEFT JOIN lane_starts ON lane_starts.process = chain.process AND lane_starts.lane = chain.lane
    UNION ALL
    SELECT chain.step, chain.process, chain.lane, 'waited' AS kind, hops.start_ns AS from_ns, hops.end_ns AS to_ns, hops.awaits
    FROM chain JOIN hops ON hops.process = chain.process AND hops.span_id = chain.wait
    WHERE hops.next_lane = hops.lane
), named AS (
    SELECT runs.step, runs.kind,
        arg_max(laned.name || coalesce(' ' || laned.function, ''), least(laned.end_ns, runs.to_ns) - greatest(laned.start_ns, runs.from_ns)) AS what,
        arg_max(laned.crate, least(laned.end_ns, runs.to_ns) - greatest(laned.start_ns, runs.from_ns)) AS crate
    FROM runs JOIN laned
        ON laned.process = runs.process AND laned.lane = runs.lane AND laned.name <> 'wait'
        AND laned.start_ns < runs.to_ns AND laned.end_ns > runs.from_ns
    GROUP BY runs.step, runs.kind
), busy AS (
    SELECT sum(least(start_ns + dur_ns, t1) - greatest(start_ns, t0)) AS busy_ns
    FROM spans, bounds
    WHERE build_id = $build AND depth = 0 AND name <> 'wait' AND start_ns < t1 AND start_ns + dur_ns > t0
)
SELECT
    runs.step,
    runs.kind,
    CASE WHEN runs.lane >= 4611686018427387904 THEN 'thread ' || CAST(runs.lane - 4611686018427387904 AS VARCHAR)
        ELSE 'job ' || CAST(runs.lane AS VARCHAR) END AS lane,
    round((runs.to_ns - runs.from_ns) / 1e6, 2) AS ms,
    coalesce(runs.awaits, named.what) AS what,
    named.crate AS crate,
    runs.from_ns,
    runs.to_ns,
    round(sum(runs.to_ns - runs.from_ns) OVER () / 1e9, 2) AS path_s,
    round(100 * sum(runs.to_ns - runs.from_ns) OVER () / (SELECT t1 - t0 FROM bounds), 1) AS path_pct,
    round((SELECT busy_ns FROM busy) / (SELECT t1 - t0 FROM bounds), 2) AS parallelism,
    max(runs.step) OVER () < 100000 AS complete
FROM runs
LEFT JOIN named ON named.step = runs.step AND named.kind = runs.kind
WHERE runs.to_ns > runs.from_ns
ORDER BY runs.from_ns, runs.step DESC
LIMIT $limit

-- name: allocations_per_query_kind
-- says: a build's allocations per query kind: bytes and allocations made while its spans were open (the queries under them too)
SELECT
    query_kind,
    count(*) AS runs,
    sum(alloc_bytes) AS alloc_bytes,
    sum(allocs) AS allocs,
    round(sum(alloc_bytes) / count(*)) AS bytes_per_run
FROM spans
WHERE build_id = $build AND query_kind IS NOT NULL
GROUP BY query_kind
ORDER BY alloc_bytes DESC NULLS LAST

-- name: what_reran
-- says: the queries a build reran, slowest first, with the input or query whose change made each rerun
SELECT kind, query_key, outcome, cause, dur_ns / 1e6 AS ms, crate, unit
FROM provenance
WHERE build_id = $build AND outcome <> 'verified'
ORDER BY dur_ns DESC
LIMIT $limit

-- name: busiest_threads
-- says: the threads that spent most of a build inside spans, by the time of their outermost spans
SELECT
    process,
    thread,
    any_value(thread_name) AS thread_name,
    sum(dur_ns) / 1e6 AS busy_ms,
    count(*) AS spans
FROM spans
WHERE build_id = $build AND depth = 0
GROUP BY process, thread
ORDER BY busy_ms DESC
LIMIT $limit

-- name: longest_gaps
-- says: the longest stretches of a build in which no span closed: stalls too short for the watchdog
WITH ends AS (
    SELECT start_ns + dur_ns AS end_ns
    FROM spans
    WHERE build_id = $build
), gaps AS (
    SELECT lag(end_ns) OVER (ORDER BY end_ns) AS from_ns, end_ns AS to_ns
    FROM ends
)
SELECT from_ns, to_ns, (to_ns - from_ns) / 1e6 AS gap_ms
FROM gaps
WHERE from_ns IS NOT NULL
ORDER BY gap_ms DESC
LIMIT $limit

-- name: top_cpu_stacks
-- says: the call stacks a build's CPU samples found most often, with the CPU seconds each stands for at 49 samples a second
SELECT
    array_to_string(stack, ' > ') AS frames,
    count(*) AS taken,
    round(count(*) / 49.0, 2) AS cpu_s
FROM samples
WHERE build_id = $build AND cpu IS DISTINCT FROM 'off'
GROUP BY stack
ORDER BY taken DESC
LIMIT $limit

-- name: time_by_span_name
-- says: a build's time per span name: how many, their whole time, their own CPU time and the longest
SELECT
    name,
    count(*) AS spans,
    sum(dur_ns) / 1e6 AS total_ms,
    sum(self_cpu_ns) / 1e6 AS self_cpu_ms,
    max(dur_ns) / 1e6 AS longest_ms
FROM spans
WHERE build_id = $build
GROUP BY name
ORDER BY total_ms DESC

-- name: frame_copies_per_query_kind
-- says: the query kinds whose task frames copied most to grow blocks in a build: moves, KiB copied, and how many of their runs copied at all
SELECT
    query_kind,
    count(*) AS runs,
    count(*) FILTER (WHERE frame_grow_copies > 0) AS runs_that_copied,
    sum(frame_grow_copies) AS copies,
    round(sum(frame_copied_bytes) / 1024, 1) AS copied_kib,
    max(frame_copied_bytes) AS most_bytes_one_run,
    round(sum(frame_copied_bytes) / sum(alloc_bytes), 4) AS copied_per_allocated_byte
FROM queries
WHERE build_id = $build AND frame_grow_copies IS NOT NULL
GROUP BY query_kind
ORDER BY sum(frame_copied_bytes) DESC
LIMIT $limit

-- name: jobs_by_stack_high_water
-- says: a build's jobs ranked by how deep they went on their fiber's stack and past it, in KiB, beside what the fiber reserved
SELECT
    rank() OVER (ORDER BY high_water DESC) AS rank,
    process,
    fiber,
    job,
    ended_ns,
    high_water / 1024 AS high_water_kib,
    deepest / 1024 AS deepest_kib,
    grown / 1024 AS grown_kib,
    touched / 1024 AS touched_kib,
    reserved / 1024 AS reserved_kib,
    round(100 * deepest / reserved, 1) AS reserved_used_pct
FROM fiber_stacks
WHERE build_id = $build
ORDER BY high_water DESC
LIMIT $limit

-- name: utilization_per_second
-- says: the standard utilization query: each second of a build, how many threads were busy (inside an outermost span) and the span names they were busy in, by seconds of thread time, those over 10 ms
WITH bounds AS (
    SELECT started_ns AS t0, started_ns + dur_ns AS t1
    FROM builds
    WHERE build_id = $build
), outer_spans AS (
    SELECT name, greatest(start_ns, t0) AS start_ns, least(start_ns + dur_ns, t1) AS end_ns, t0
    FROM spans, bounds
    WHERE depth = 0 AND start_ns < t1 AND start_ns + dur_ns > t0
), seconds AS (
    SELECT unnest(range(0, cast((t1 - t0) / 1e9 AS BIGINT) + 1)) AS second
    FROM bounds
), by_name AS (
    SELECT
        second,
        name,
        sum(least(end_ns, t0 + (second + 1) * 1000000000) - greatest(start_ns, t0 + second * 1000000000)) AS ns
    FROM seconds
    JOIN outer_spans ON start_ns < t0 + (second + 1) * 1000000000 AND end_ns > t0 + second * 1000000000
    GROUP BY second, name
)
SELECT
    seconds.second,
    round(coalesce(sum(ns), 0) / 1e9, 2) AS busy_threads,
    string_agg(name || ' ' || round(ns / 1e9, 2), ', ' ORDER BY ns DESC) FILTER (WHERE ns >= 10000000) AS busy_in
FROM seconds
LEFT JOIN by_name USING (second)
GROUP BY seconds.second
ORDER BY seconds.second

-- name: uncovered_time
-- says: how much of a build's wall time no thread was inside any span: what its profile cannot account for, which should stay under 5%
WITH bounds AS (
    SELECT started_ns AS t0, started_ns + dur_ns AS t1
    FROM builds
    WHERE build_id = $build
), outer_spans AS (
    SELECT greatest(start_ns, t0) AS start_ns, least(start_ns + dur_ns, t1) AS end_ns
    FROM spans, bounds
    WHERE depth = 0 AND start_ns < t1 AND start_ns + dur_ns > t0
    UNION ALL
    SELECT t1, t1 FROM bounds
), swept AS (
    SELECT
        start_ns,
        coalesce(
            max(end_ns) OVER (ORDER BY start_ns, end_ns ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING),
            (SELECT t0 FROM bounds)
        ) AS covered_to
    FROM outer_spans
)
SELECT
    round(any_value(t1 - t0) / 1e9, 2) AS wall_s,
    round(sum(greatest(start_ns - covered_to, 0)) / 1e9, 2) AS uncovered_s,
    round(100 * sum(greatest(start_ns - covered_to, 0)) / any_value(t1 - t0), 1) AS uncovered_pct
FROM swept, bounds

-- name: worker_states
-- says: each pool worker's every moment of a build, from the pool's own switches: running a job (which), idle (why: nothing to run, or a timer until), or the driver between them
WITH bounds AS (
    SELECT started_ns AS t0, started_ns + dur_ns AS t1
    FROM builds
    WHERE build_id = $build
), switched AS (
    SELECT process, thread, thread_name, t_ns, event, job, wait_kind,
        lead(t_ns) OVER (PARTITION BY process, thread ORDER BY t_ns) AS next_ns
    FROM switches, bounds
    WHERE t_ns BETWEEN t0 AND t1
)
SELECT process, thread, thread_name,
    CASE WHEN event IN ('started', 'resumed') THEN 'run' WHEN event = 'idle' THEN 'idle' ELSE 'driver' END AS state,
    CASE WHEN event IN ('started', 'resumed') THEN job END AS job,
    CASE WHEN event = 'idle' THEN wait_kind END AS reason,
    t_ns AS start_ns,
    coalesce(next_ns, (SELECT t1 FROM bounds)) AS end_ns
FROM switched
ORDER BY thread, start_ns

-- name: uncovered_per_worker
-- says: per pool worker, the share of a build's wall time it ran a job (from the job's start or resume to its next switch) or was in no state the pool said (before its first switch) and was inside no span: every job opens a span of its own, so the gate holds it under 1%
WITH bounds AS (
    SELECT started_ns AS t0, started_ns + dur_ns AS t1
    FROM builds
    WHERE build_id = $build
), workers AS (
    SELECT DISTINCT process, thread, any_value(thread_name) OVER (PARTITION BY process, thread) AS thread_name
    FROM switches, bounds
    WHERE t_ns BETWEEN t0 AND t1
), switched AS (
    SELECT switches.process, switches.thread, t_ns, event,
        lead(t_ns, 1, (SELECT t1 FROM bounds)) OVER (PARTITION BY switches.process, switches.thread ORDER BY t_ns) AS next_ns
    FROM switches, bounds
    WHERE t_ns BETWEEN t0 AND t1
), covered AS (
    -- What the pool said the worker did besides running a job: idle, or between a job's park
    -- or end and the next job's start or resume.
    SELECT process, thread, t_ns AS start_ns, next_ns AS end_ns
    FROM switched
    WHERE event NOT IN ('started', 'resumed')
    UNION ALL
    SELECT spans.process, spans.thread, greatest(start_ns, t0), least(start_ns + dur_ns, t1)
    FROM spans, bounds
    WHERE depth = 0 AND start_ns < t1 AND start_ns + dur_ns > t0
), swept AS (
    SELECT process, thread, start_ns, end_ns,
        coalesce(
            max(end_ns) OVER (PARTITION BY process, thread ORDER BY start_ns, end_ns ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING),
            (SELECT t0 FROM bounds)
        ) AS covered_to
    FROM covered
), gaps AS (
    SELECT process, thread,
        sum(greatest(start_ns - covered_to, 0)) + greatest((SELECT t1 FROM bounds) - max(greatest(end_ns, covered_to)), 0) AS uncovered_ns
    FROM swept
    GROUP BY process, thread
)
SELECT workers.thread, workers.thread_name,
    round(coalesce(uncovered_ns, (SELECT t1 - t0 FROM bounds)) / 1e9, 2) AS uncovered_s,
    round(100 * coalesce(uncovered_ns, (SELECT t1 - t0 FROM bounds)) / (SELECT t1 - t0 FROM bounds), 2) AS uncovered_pct
FROM workers LEFT JOIN gaps USING (process, thread)
ORDER BY uncovered_pct DESC

-- name: raw_blocks
-- says: samples of a build's pool workers blocked in the kernel (a mutex, a condition, kevent, a read) with no open wait span saying on what: each is a block that bypasses blaze_graph::wait, and the gate fails on any
WITH RECURSIVE bounds AS (
    SELECT started_ns AS t0, started_ns + dur_ns AS t1
    FROM builds
    WHERE build_id = $build
), workers AS (
    SELECT DISTINCT process, thread
    FROM switches, bounds
    WHERE t_ns BETWEEN t0 AND t1
), blocked AS (
    SELECT samples.process, samples.thread, samples.t_ns, samples.span_id, array_to_string(samples.stack, ' > ') AS frames
    FROM samples
    JOIN workers USING (process, thread)
    CROSS JOIN bounds
    WHERE samples.t_ns BETWEEN t0 AND t1
        AND regexp_matches(array_to_string(samples.stack, ' > '), '(psynch_mutexwait|psynch_cvwait|kevent|__semwait_signal|(^| > )(read|__read_nocancel)\+)')
), up AS (
    SELECT blocked.process, blocked.span_id AS sampled, spans.span_id, spans.parent, spans.name
    FROM blocked JOIN spans ON spans.process = blocked.process AND spans.span_id = blocked.span_id
    UNION ALL
    SELECT up.process, up.sampled, spans.span_id, spans.parent, spans.name
    FROM up JOIN spans ON spans.process = up.process AND spans.span_id = up.parent
)
SELECT blocked.thread, blocked.t_ns, blocked.span_id, blocked.frames
FROM blocked
WHERE NOT EXISTS (
    SELECT 1 FROM up WHERE up.process = blocked.process AND up.sampled = blocked.span_id AND up.name = 'wait'
)
ORDER BY blocked.t_ns
LIMIT $limit

-- name: derive_panics_per_build
-- says: how many derive proc macros panicked in each of the last builds, and how many distinct macros and places
SELECT
    builds.build_id,
    builds.commit,
    count(errors.kind) AS derive_panics,
    count(DISTINCT errors.function) AS macros,
    count(DISTINCT errors.file || ':' || errors.line) AS places
FROM builds
LEFT JOIN errors ON errors.build_id = builds.build_id AND errors.kind = 'derive-panic'
GROUP BY builds.build_id, builds.commit
ORDER BY builds.build_id DESC
LIMIT $limit

-- name: errors_by_kind
-- says: a build's errors per kind: how many, in how many crates, and the commonest reason (ties by its text)
WITH reasons AS (
    SELECT kind, crate, coalesce(detail, message) AS reason
    FROM errors
    WHERE build_id = $build
), counted AS (
    SELECT kind, reason, count(*) AS n
    FROM reasons
    GROUP BY kind, reason
)
SELECT
    kind,
    count(*) AS errors,
    count(DISTINCT crate) AS crates,
    (SELECT counted.reason FROM counted WHERE counted.kind = reasons.kind ORDER BY counted.n DESC, counted.reason LIMIT 1) AS commonest
FROM reasons
GROUP BY kind
ORDER BY errors DESC, kind

-- name: reexecution_per_query_kind
-- says: per query kind in a build, the work repeated: runs, rows verified without running and runs of a key already run (the query engine's counters), beside its spans' runs, distinct keys, repeats and time
WITH counted AS (
    SELECT query_kind,
        max(value) FILTER (WHERE name = 'query executed') AS executed,
        max(value) FILTER (WHERE name = 'query verified') AS verified,
        max(value) FILTER (WHERE name = 'query reran') AS reran
    FROM counters
    WHERE build_id = $build AND query_kind IS NOT NULL
    GROUP BY query_kind
), spanned AS (
    SELECT query_kind, count(*) AS span_runs, count(DISTINCT function) AS span_keys, sum(dur_ns) AS span_ns
    FROM spans
    WHERE build_id = $build AND query_kind IS NOT NULL
    GROUP BY query_kind
), kinds AS (
    SELECT query_kind FROM counted
    UNION
    SELECT query_kind FROM spanned
)
SELECT
    kinds.query_kind AS query_kind,
    CAST(counted.executed AS BIGINT) AS executed,
    CAST(counted.verified AS BIGINT) AS verified,
    CAST(counted.reran AS BIGINT) AS reran,
    spanned.span_runs AS span_runs,
    spanned.span_keys AS span_keys,
    CASE WHEN spanned.span_keys > 0 THEN spanned.span_runs - spanned.span_keys END AS span_repeats,
    round(spanned.span_ns / 1e6, 1) AS span_ms
FROM kinds
LEFT JOIN counted ON counted.query_kind = kinds.query_kind
LEFT JOIN spanned ON spanned.query_kind = kinds.query_kind
ORDER BY coalesce(CAST(counted.reran AS BIGINT), spanned.span_runs - spanned.span_keys) DESC NULLS LAST
LIMIT $limit

-- name: footprint_per_build
-- says: per build, newest first, the process's physical footprint in its window (the counter `footprint bytes`: all the OS counts against the daemon), its peak and its last value, beside the most `blaze_mem`'s tags held (`live bytes`), in MiB; null where the build has no such counter
SELECT
    builds.build_id,
    round(max(counters.value) FILTER (WHERE counters.name = 'footprint bytes') / 1048576, 1) AS peak_footprint_mib,
    round(arg_max(counters.value, counters.t_ns) FILTER (WHERE counters.name = 'footprint bytes') / 1048576, 1) AS last_footprint_mib,
    round(max(counters.value) FILTER (WHERE counters.name = 'live bytes') / 1048576, 1) AS peak_live_mib
FROM builds
LEFT JOIN counters ON counters.t_ns >= builds.started_ns AND counters.t_ns <= builds.started_ns + builds.dur_ns
GROUP BY builds.build_id
ORDER BY builds.build_id DESC
LIMIT $limit
