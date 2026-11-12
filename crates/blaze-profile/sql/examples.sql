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
-- says: the spans whose median time grew most from the commit `earlier` to the commit `later` (prefixes of either), over each build's summary (`build_summary`): a span's median is the median of its builds' medians, so one long build counts once
WITH earlier AS (
    SELECT name, median(median_ns) AS dur_ns, sum(runs) AS runs, count(*) AS builds
    FROM build_summary
    WHERE starts_with(commit, $earlier)
    GROUP BY name
), later AS (
    SELECT name, median(median_ns) AS dur_ns, sum(runs) AS runs, count(*) AS builds
    FROM build_summary
    WHERE starts_with(commit, $later)
    GROUP BY name
)
SELECT
    later.name AS name,
    earlier.dur_ns / 1e6 AS earlier_ms,
    later.dur_ns / 1e6 AS later_ms,
    (later.dur_ns - earlier.dur_ns) / 1e6 AS slower_ms,
    earlier.runs AS earlier_runs,
    later.runs AS later_runs,
    earlier.builds AS earlier_builds,
    later.builds AS later_builds
FROM later
JOIN earlier ON earlier.name = later.name
ORDER BY slower_ms DESC
LIMIT $limit

-- name: memo_hit_rate_per_query_kind
-- says: per query function in a build, what the memo kept and what ran and why (`query_counts`): rows verified without running, runs, and runs by cause (a first run, a read changed, an untracked read, a cycle, an evicted value asked for again), with the share kept; null causes are a history from before schema 4
SELECT
    query,
    verified,
    executed,
    ran_new,
    ran_changed,
    ran_untracked,
    ran_cycle,
    evicted_asked,
    round(verified / greatest(verified + executed, 1), 3) AS hit_rate
FROM query_counts
WHERE build_id = $build
ORDER BY coalesce(ran_changed, 0) + coalesce(ran_untracked, 0) + coalesce(ran_cycle, 0) + evicted_asked DESC, executed DESC, query
LIMIT $limit

-- name: critical_path
-- says: a build's critical path, first step first: from the work that ended the build back through each wait to whoever ran what it waited on (a wait's owner: a job as `switches.job` names it, generation and slot, the numbering since schema 2), each step what ran on which lane (a fiber job, or a thread) and for how long, a wait nobody owns a step of its own; on every row the whole path's seconds, its share of the wall time (near 100: the path bounds the build) the build's parallelism (busy threads on average), and whether the walk reached the build's start within its bound of 100000 steps (false: the path is cut short)
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
    SELECT process, job, min(t_ns) AS t_ns
    FROM switches, bounds
    WHERE event = 'started' AND t_ns <= t1
    GROUP BY process, job
), owned AS (
    -- A wait's `owner` is its runner's job as the switches name it (generation << 32 | slot),
    -- so the owner's lane is that job's, if it has one: a runner on no fiber (a thread's own
    -- job) has no switches, and the wait stays on its lane. Until 2026-11-11 this matched the
    -- owner as schema 1 numbered it (slot + 1), which no file since schema 2 holds: the walk
    -- crossed no wait.
    SELECT waits.*,
        CASE WHEN job_starts.t_ns <= waits.end_ns THEN job_starts.job END AS owner_lane
    FROM waits LEFT JOIN job_starts ON job_starts.process = waits.process AND job_starts.job = waits.owner
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
-- says: a build's allocations per query kind: bytes and allocations made while its spans were open (the queries under them too), folded runs counted
SELECT
    query_kind,
    sum(runs) AS runs,
    sum(alloc_bytes) AS alloc_bytes,
    sum(allocs) AS allocs,
    round(sum(alloc_bytes) / sum(runs)) AS bytes_per_run
FROM span_sums
WHERE build_id = $build AND query_kind IS NOT NULL
GROUP BY query_kind
ORDER BY alloc_bytes DESC NULLS LAST

-- name: what_reran
-- says: the memos a build ran again, each with its cause and the read whose change made it run (`provenance`); first runs are only counted (`query_counts.ran_new`), verified rows too
SELECT kind, query_key, cause, view, cause_key, cause_kind
FROM provenance
WHERE build_id = $build AND outcome = 'ran'
ORDER BY kind, query_key, view
LIMIT $limit

-- name: why_reran
-- says: what a build's reruns come down to: each memo's chain of changed reads followed through the memos that changed to the input field, untracked read or cycle at its root, and how many memos each root made run again
WITH RECURSIVE ran AS (
    SELECT DISTINCT query_key, cause, cause_key, cause_kind
    FROM provenance
    WHERE build_id = $build AND outcome = 'ran'
), chain AS (
    SELECT query_key AS memo, cause, cause_key, cause_kind, 0 AS depth
    FROM ran
    UNION ALL
    SELECT chain.memo AS memo, ran.cause AS cause, ran.cause_key AS cause_key, ran.cause_kind AS cause_kind, chain.depth + 1 AS depth
    FROM chain
    JOIN ran ON ran.query_key = chain.cause_key
    WHERE chain.cause_kind = 'function' AND chain.depth < 64
), roots AS (
    SELECT memo, arg_max(cause, depth) AS root_cause, arg_max(cause_key, depth) AS root, arg_max(cause_kind, depth) AS root_kind, max(depth) AS depth
    FROM chain
    GROUP BY memo
)
SELECT root_cause, root_kind, root, count(DISTINCT memo) AS memos, max(depth) AS deepest, min(memo) AS example
FROM roots
GROUP BY root_cause, root_kind, root
ORDER BY memos DESC, root_cause, root
LIMIT $limit

-- name: untracked_reads
-- says: a build's untracked reads by name (`Untracked { what }`): each memo that made one runs again in every view, so these are the reruns no input change explains
SELECT what, kind, count(DISTINCT query_key) AS memos, min(query_key) AS example
FROM provenance
WHERE build_id = $build AND outcome = 'untracked read'
GROUP BY what, kind
ORDER BY memos DESC, what, kind
LIMIT $limit

-- name: cycle_reruns
-- says: the memos a build ran again for a cycle, with their runs and views: one that ran many times in one view is a cycle that took many rounds to settle, or never did
SELECT kind, query_key, count(*) AS runs, count(DISTINCT view) AS views
FROM provenance
WHERE build_id = $build AND outcome = 'ran' AND cause = 'cycle'
GROUP BY kind, query_key
ORDER BY runs DESC, kind, query_key
LIMIT $limit

-- name: key_part_diff
-- says: why a thing missed in build `after`: the parts of its key (`key_parts`) whose hash differs from the last earlier build at or before `before` that computed it too, or that only one of the two has
WITH later AS (
    SELECT step, item, part, hash, names, beyond FROM key_parts WHERE build_id = $after
), latest AS (
    SELECT step, item, max(build_id) AS build
    FROM key_parts
    WHERE build_id <= $before AND item IN (SELECT item FROM later)
    GROUP BY step, item
), earlier AS (
    SELECT key_parts.step AS step, key_parts.item AS item, part, hash, names, beyond, build_id
    FROM key_parts
    JOIN latest ON latest.step = key_parts.step AND latest.item = key_parts.item AND latest.build = key_parts.build_id
)
SELECT
    coalesce(later.step, earlier.step) AS step,
    coalesce(later.item, earlier.item) AS item,
    coalesce(later.part, earlier.part) AS part,
    earlier.build_id AS earlier_build,
    earlier.hash AS earlier_hash,
    later.hash AS later_hash,
    coalesce(later.names, earlier.names) AS names,
    coalesce(later.beyond, earlier.beyond) AS beyond
FROM earlier
FULL JOIN later
    ON later.step = earlier.step AND later.item = earlier.item AND later.part IS NOT DISTINCT FROM earlier.part
WHERE earlier.hash IS DISTINCT FROM later.hash
    AND coalesce(later.item, earlier.item) IN (SELECT item FROM earlier)
ORDER BY step, item, part
LIMIT $limit

-- name: task_memory
-- says: a build's daemon tasks by kind (`task` spans): how many ran, the memory the plane granted them against the most they held, how many held more than granted and by how much (over), how many held under half their grant and what of it went unused (under), how many memory held back, and their time
SELECT
    function AS task,
    count(*) AS tasks,
    round(sum(granted_bytes) / 1048576, 1) AS granted_mib,
    round(sum(peak_bytes) / 1048576, 1) AS peak_mib,
    round(max(peak_bytes) / 1048576, 1) AS largest_peak_mib,
    count(*) FILTER (WHERE peak_bytes > granted_bytes) AS over_grant,
    round(sum(peak_bytes - granted_bytes) FILTER (WHERE peak_bytes > granted_bytes) / 1048576, 1) AS over_mib,
    count(*) FILTER (WHERE peak_bytes * 2 < granted_bytes) AS under_grant,
    round(sum(granted_bytes - peak_bytes) FILTER (WHERE peak_bytes * 2 < granted_bytes) / 1048576, 1) AS under_mib,
    count(*) FILTER (WHERE passed > 0) AS held_back,
    max(passed) AS most_passed,
    round(sum(dur_ns) / 1e6, 1) AS ms
FROM spans
WHERE build_id = $build AND name = 'task'
GROUP BY function
ORDER BY peak_mib DESC NULLS LAST, task

-- name: task_grants
-- says: a build's daemon tasks whose peak passed their grant or stayed under half of it (`task` spans), worst first: the task kind, its crate and unit, its grant and peak, and by how much the grant missed
SELECT
    function AS task,
    crate,
    unit,
    round(granted_bytes / 1048576, 1) AS granted_mib,
    round(peak_bytes / 1048576, 1) AS peak_mib,
    CASE WHEN peak_bytes > granted_bytes THEN 'over' ELSE 'under' END AS missed,
    round(abs(peak_bytes::DOUBLE - granted_bytes::DOUBLE) / 1048576, 1) AS missed_mib
FROM spans
WHERE build_id = $build AND name = 'task'
    AND (peak_bytes > granted_bytes OR peak_bytes * 2 < granted_bytes)
ORDER BY missed_mib DESC, task, crate, unit
LIMIT $limit

-- name: memory_timeline
-- says: a build's memory minute by minute from its first counter (`blaze explain memory`): the footprint (all the OS counts against the daemon), the global heap's live blocks, what blaze-mem's tags hold, what the allocators keep freed (`allocator kept bytes`, or the footprint less the live heap and blaze-mem's resident pages where no such row is), and the most tasks running, in GiB; each column its minute's most. Counters of a blaze before they carried a build are taken by the build's window
WITH c AS (
    SELECT t_ns, name, value FROM counters
    WHERE build_id = $build
        OR (build_id IS NULL
            AND NOT EXISTS (SELECT 1 FROM counters WHERE build_id = $build)
            AND t_ns BETWEEN (SELECT started_ns FROM builds WHERE build_id = $build)
                AND (SELECT started_ns + coalesce(dur_ns, 0) FROM builds WHERE build_id = $build))
),
measured AS (SELECT t_ns, value AS kept FROM c WHERE name = 'allocator kept bytes'),
derived AS (
    SELECT r.t_ns, f.value - h.value - r.value AS kept
    FROM (SELECT t_ns, value FROM c WHERE name = 'blaze-mem resident bytes') AS r
    ASOF JOIN (SELECT t_ns, value FROM c WHERE name = 'heap live bytes') AS h ON r.t_ns >= h.t_ns
    ASOF JOIN (SELECT t_ns, value FROM c WHERE name = 'footprint bytes') AS f ON r.t_ns >= f.t_ns
),
kept AS (
    SELECT * FROM measured
    UNION ALL
    SELECT * FROM derived WHERE NOT EXISTS (SELECT 1 FROM measured)
),
points AS (
    SELECT t_ns, name, value FROM c
    WHERE name IN ('footprint bytes', 'heap live bytes', 'live bytes', 'pool running')
    UNION ALL
    SELECT t_ns, 'kept', kept FROM kept
)
SELECT
    floor((t_ns - (SELECT min(t_ns) FROM c)) / 60e9)::BIGINT AS minute,
    round(max(value) FILTER (WHERE name = 'footprint bytes') / 1073741824, 2) AS footprint_gib,
    round(max(value) FILTER (WHERE name = 'heap live bytes') / 1073741824, 2) AS heap_live_gib,
    round(max(value) FILTER (WHERE name = 'live bytes') / 1073741824, 2) AS blaze_mem_gib,
    round(max(value) FILTER (WHERE name = 'kept') / 1073741824, 2) AS kept_gib,
    CAST(max(value) FILTER (WHERE name = 'pool running') AS BIGINT) AS tasks_running
FROM points
GROUP BY minute
ORDER BY minute

-- name: memory_peak_tasks
-- says: a build's daemon tasks that held the most at once (`task` spans, `blaze explain memory`), largest first: the task kind, its crate and unit (the task span's, else the unit phase span under it), the most it held, its grant, what it allocated and freed of the global heap in all and how many times its peak that churn is, and how long it ran
WITH tasks AS (
    SELECT * FROM spans WHERE build_id = $build AND name = 'task' AND peak_bytes IS NOT NULL
),
phases AS (
    SELECT process, parent, any_value(crate) AS krate, any_value(unit) AS unit
    FROM spans
    WHERE build_id = $build
        AND name IN ('unit names', 'unit bodies', 'unit codegen', 'unit link', 'unit store')
    GROUP BY process, parent
)
SELECT
    t.function AS task,
    coalesce(t.crate, p.krate) AS crate,
    coalesce(t.unit, p.unit) AS unit,
    round(t.peak_bytes / 1073741824, 2) AS peak_gib,
    round(t.granted_bytes / 1073741824, 2) AS granted_gib,
    round(t.heap_allocated_bytes / 1073741824, 2) AS heap_allocated_gib,
    round(t.heap_freed_bytes / 1073741824, 2) AS heap_freed_gib,
    round(t.heap_allocated_bytes / nullif(t.peak_bytes, 0), 1) AS churn_x_peak,
    round(t.dur_ns / 1e9, 1) AS s
FROM tasks AS t
LEFT JOIN phases AS p ON p.process = t.process AND p.parent = t.span_id
ORDER BY t.peak_bytes DESC, task
LIMIT $limit

-- name: memory_at_peak
-- says: a build's memory by area at the instant its footprint peaked (`blaze explain memory`), most first: each tag's blaze-mem bytes and global-heap bytes then, and what it allocated and freed of the global heap from the build's first counter to the peak; then the process's own areas (the footprint, the live heap and what its allocator committed, what the allocators keep, blaze-mem's resident pages, free chunks and kept values' classes), in GiB
WITH c AS (
    SELECT t_ns, name, value FROM counters
    WHERE build_id = $build
        OR (build_id IS NULL
            AND NOT EXISTS (SELECT 1 FROM counters WHERE build_id = $build)
            AND t_ns BETWEEN (SELECT started_ns FROM builds WHERE build_id = $build)
                AND (SELECT started_ns + coalesce(dur_ns, 0) FROM builds WHERE build_id = $build))
),
peak AS (
    SELECT t_ns FROM c WHERE name = 'footprint bytes' ORDER BY value DESC, t_ns LIMIT 1
),
then_ AS (
    SELECT name, arg_max(value, t_ns) AS value FROM c
    WHERE t_ns <= (SELECT t_ns FROM peak)
    GROUP BY name
),
first_ AS (SELECT name, arg_min(value, t_ns) AS value FROM c GROUP BY name),
tags AS (
    SELECT
        split_part(then_.name, ': ', 2) AS area,
        max(then_.value) FILTER (WHERE then_.name LIKE 'tag live bytes: %') AS mem,
        max(then_.value) FILTER (WHERE then_.name LIKE 'tag heap bytes: %') AS heap,
        max(then_.value - first_.value) FILTER (WHERE then_.name LIKE 'tag heap allocated bytes: %') AS allocated,
        max(then_.value - first_.value) FILTER (WHERE then_.name LIKE 'tag heap freed bytes: %') AS freed
    FROM then_ JOIN first_ USING (name)
    WHERE then_.name LIKE 'tag %: %'
    GROUP BY area
),
process AS (
    SELECT name AS area, value AS mem FROM then_
    WHERE name IN ('footprint bytes', 'heap live bytes', 'heap committed bytes', 'allocator kept bytes',
        'blaze-mem resident bytes', 'chunks warm bytes', 'chunks cold bytes', 'blocks live bytes',
        'blocks warm bytes', 'blocks cold bytes')
)
SELECT
    area,
    'tag' AS kind,
    round((coalesce(mem, 0) + coalesce(heap, 0)) / 1073741824, 2) AS held_gib,
    round(mem / 1073741824, 2) AS blaze_mem_gib,
    round(heap / 1073741824, 2) AS heap_gib,
    round(allocated / 1073741824, 2) AS heap_allocated_gib,
    round(freed / 1073741824, 2) AS heap_freed_gib
FROM tags
UNION ALL
SELECT replace(area, ' bytes', ''), 'process', round(mem / 1073741824, 2), NULL, NULL, NULL, NULL
FROM process
ORDER BY kind DESC, held_gib DESC NULLS LAST, area

-- name: memory_held
-- says: what a build's global heap held, by call site (`held_samples`: the sampled blocks nobody had freed): the daemon's last reading at or before the build's footprint peaked (its first, where none was that early), each site's bytes held (its samples' weights summed, within a sample's), its share, the tags of the tasks that allocated there, its samples and their mean block size, and how long ago the oldest was allocated, most first
WITH peak AS (
    SELECT process, arg_max(t_ns, value) AS t_ns FROM counters
    WHERE build_id = $build AND name = 'footprint bytes' GROUP BY process
    ORDER BY max(value) DESC LIMIT 1
),
reading AS (
    SELECT coalesce(max(read_ns) FILTER (WHERE read_ns <= (SELECT t_ns FROM peak)), min(read_ns)) AS read_ns
    FROM held_samples
    WHERE process = (SELECT process FROM peak)
),
s AS (SELECT * FROM held_samples WHERE read_ns = (SELECT read_ns FROM reading))
SELECT
    coalesce(site, '(in the allocator)') AS site,
    round(sum(bytes) / 1073741824, 2) AS held_gib,
    round(100.0 * sum(bytes) / (SELECT sum(bytes) FROM s), 1) AS pct,
    string_agg(DISTINCT coalesce(tag, 'outside'), ', ' ORDER BY coalesce(tag, 'outside')) AS tags,
    count(*) AS samples,
    CAST(avg(block_bytes) AS BIGINT) AS mean_block_bytes,
    round((max(read_ns) - min(t_ns)) / 60e9, 1) AS oldest_min
FROM s
GROUP BY site
ORDER BY held_gib DESC, site
LIMIT $limit

-- name: memory_held_now
-- says: what the global heap holds now, by call site (`held_samples`: the sampled blocks nobody has freed), from the newest reading of the newest daemon that has one (a reading a minute, so an idle daemon's is of its idle heap): each site's bytes held, its share, the tags of the tasks that allocated there, its samples and their mean block size, and how long ago the oldest was allocated, most first
WITH reading AS (
    SELECT process, read_ns FROM held_samples ORDER BY read_ns DESC LIMIT 1
),
s AS (
    SELECT * FROM held_samples
    WHERE read_ns = (SELECT read_ns FROM reading) AND process = (SELECT process FROM reading)
)
SELECT
    coalesce(site, '(in the allocator)') AS site,
    round(sum(bytes) / 1048576, 0) AS held_mib,
    round(100.0 * sum(bytes) / (SELECT sum(bytes) FROM s), 1) AS pct,
    string_agg(DISTINCT coalesce(tag, 'outside'), ', ' ORDER BY coalesce(tag, 'outside')) AS tags,
    count(*) AS samples,
    CAST(avg(block_bytes) AS BIGINT) AS mean_block_bytes,
    round((max(read_ns) - min(t_ns)) / 60e9, 1) AS oldest_min
FROM s
GROUP BY site
ORDER BY held_mib DESC, site
LIMIT $limit

-- name: memory_sites
-- says: what churns a build's global heap, by call site (`alloc_samples`, `blaze explain memory`): each site's bytes allocated (its samples' weights summed, within a sample's), its share of the build's, the tags of the tasks that allocated there, its samples and their mean block size, most first
WITH s AS (SELECT * FROM alloc_samples WHERE build_id = $build)
SELECT
    coalesce(site, '(in the allocator)') AS site,
    round(sum(bytes) / 1073741824, 2) AS allocated_gib,
    round(100.0 * sum(bytes) / (SELECT sum(bytes) FROM s), 1) AS pct,
    string_agg(DISTINCT coalesce(tag, 'outside'), ', ' ORDER BY coalesce(tag, 'outside')) AS tags,
    count(*) AS samples,
    CAST(avg(block_bytes) AS BIGINT) AS mean_block_bytes
FROM s
GROUP BY site
ORDER BY allocated_gib DESC, site
LIMIT $limit

-- name: admission_waits
-- says: what a build's daemon tasks waited for before they started (`admission` spans, from ready to start): per task kind and what held it back (memory, a work or io thread, nothing), how many, their whole and longest wait, and the times memory passed them over; a null waited_for never started
SELECT
    function AS task,
    awaits AS waited_for,
    count(*) AS tasks,
    round(sum(dur_ns) / 1e6, 1) AS waited_ms,
    round(max(dur_ns) / 1e6, 1) AS longest_ms,
    sum(passed) AS passed,
    round(max(granted_bytes) / 1048576, 1) AS largest_grant_mib
FROM spans
WHERE build_id = $build AND name = 'admission'
GROUP BY function, awaits
ORDER BY waited_ms DESC, task, waited_for
LIMIT $limit

-- name: busiest_threads
-- says: the threads that spent most of a build inside spans, by the time of their outermost spans, folded ones counted
SELECT
    process,
    thread,
    any_value(thread_name) AS thread_name,
    sum(dur_ns) / 1e6 AS busy_ms,
    sum(runs) AS spans
FROM span_sums
WHERE build_id = $build AND root
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
-- says: a build's time per span name: how many, their whole time, their own CPU time and the longest, folded spans counted
SELECT
    name,
    sum(runs) AS spans,
    sum(dur_ns) / 1e6 AS total_ms,
    sum(self_cpu_ns) / 1e6 AS self_cpu_ms,
    max(max_dur_ns) / 1e6 AS longest_ms
FROM span_sums
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
-- says: how much of a build's wall time no thread was inside any span: what its profile cannot account for, which should stay under 1% (`gate::UNCOVERED_MAX_PCT`)
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
-- says: per pool worker, the share of a build's wall time it was inside no span and not idle: every job opens a `job` span and the worker's own search and switches between jobs a `worker` span, so the gate holds it under 1%
WITH bounds AS (
    SELECT started_ns AS t0, started_ns + dur_ns AS t1
    FROM builds
    WHERE build_id = $build
), workers AS (
    -- A pool worker runs jobs or sleeps; a thread that only wakes jobs (`ready`) is not one.
    SELECT DISTINCT process, thread, any_value(thread_name) OVER (PARTITION BY process, thread) AS thread_name
    FROM switches, bounds
    WHERE t_ns BETWEEN t0 AND t1 AND event <> 'ready'
), switched AS (
    -- From before the build too: a worker idle as the build began is idle from its start.
    SELECT switches.process, switches.thread, t_ns, event, since_ns,
        lead(t_ns, 1, (SELECT t1 FROM bounds)) OVER (PARTITION BY switches.process, switches.thread ORDER BY t_ns) AS next_ns
    FROM switches JOIN workers USING (process, thread), bounds
    WHERE t_ns <= t1
), covered AS (
    -- Idle as the pool said it: an idle span the build's end left open is not in the history.
    SELECT process, thread, greatest(t_ns, t0) AS start_ns, least(next_ns, t1) AS end_ns
    FROM switched, bounds
    WHERE event = 'idle' AND next_ns > t0
    UNION ALL
    -- A job's run: its spans move with it and are written on the thread they close on, so a
    -- span begun on another thread, or still open, covers this one's stretch only through here.
    SELECT process, thread, greatest(t_ns, t0), least(next_ns, t1)
    FROM switched, bounds
    WHERE event IN ('started', 'resumed') AND next_ns > t0
    UNION ALL
    -- And its run before it joined the graph, from its begin on this thread.
    SELECT process, thread, greatest(since_ns, t0), least(t_ns, t1)
    FROM switched, bounds
    WHERE event IN ('started', 'ran') AND t_ns > t0 AND since_ns < t1
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
), folded_roots AS (
    -- Outermost spans too short for a row each (`folded`): their time is covered, though no
    -- row says where in the stretch it fell; they lie between the runs above, in its gaps.
    SELECT process, thread, sum(dur_ns) AS folded_ns
    FROM folded, bounds
    WHERE root = 1 AND to_ns BETWEEN t0 AND t1
    GROUP BY process, thread
), uncovered AS (
    SELECT workers.thread, workers.thread_name,
        greatest(coalesce(uncovered_ns, (SELECT t1 - t0 FROM bounds)) - coalesce(folded_ns, 0), 0) AS ns
    FROM workers LEFT JOIN gaps USING (process, thread) LEFT JOIN folded_roots USING (process, thread)
)
SELECT thread, thread_name,
    round(ns / 1e9, 2) AS uncovered_s,
    round(100 * ns / (SELECT t1 - t0 FROM bounds), 2) AS uncovered_pct
FROM uncovered
ORDER BY uncovered_pct DESC

-- name: raw_blocks
-- says: samples of a build's pool workers blocked (off the CPU: a mutex, a condition, a poll, a read) with no open wait span saying on what: each is a block that bypasses blaze_graph::wait, and the gate fails on any
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
        AND samples.cpu = 'off'
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
-- says: per query function in a build, the work repeated and what eviction cost: runs, rows verified without running, runs of a row in a view it already ran in, runs for an evicted value, and values a budget's pass let go or dropped (the query engine's counts, `query_counts`), beside its spans' runs, distinct function names and time; a span's `function` is a display name several bodies share, so span_names undercounts keys
WITH spanned AS (
    SELECT sums.query_kind, sums.span_runs, named.span_names, sums.span_ns
    FROM (
        SELECT query_kind, sum(runs) AS span_runs, sum(dur_ns) AS span_ns
        FROM span_sums
        WHERE build_id = $build AND query_kind IS NOT NULL
        GROUP BY query_kind
    ) AS sums
    LEFT JOIN (
        -- A folded span has no `function`: the names are of the spans with a row.
        SELECT query_kind, count(DISTINCT function) AS span_names
        FROM spans
        WHERE build_id = $build AND query_kind IS NOT NULL
        GROUP BY query_kind
    ) AS named USING (query_kind)
), counted AS (
    SELECT * FROM query_counts WHERE build_id = $build
), kinds AS (
    SELECT query AS query_kind FROM counted
    UNION
    SELECT query_kind FROM spanned
)
SELECT
    kinds.query_kind AS query_kind,
    counted.executed AS executed,
    counted.verified AS verified,
    counted.reran AS reran,
    counted.evicted_asked AS evicted_asked,
    counted.evicted AS evicted,
    counted.dropped AS dropped,
    spanned.span_runs AS span_runs,
    spanned.span_names AS span_names,
    round(spanned.span_ns / 1e6, 1) AS span_ms
FROM kinds
LEFT JOIN counted ON counted.query = kinds.query_kind
LEFT JOIN spanned ON spanned.query_kind = kinds.query_kind
ORDER BY coalesce(counted.reran + counted.evicted_asked, spanned.span_runs - spanned.span_names) DESC NULLS LAST
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

-- name: link_regions
-- says: each region of each image a build linked, by address, as the stage that laid it out said it (`link region` events, crates/blaze-link/src/shape.rs): its section and class, address, bytes placed, room, and the bytes unused before it
WITH regions AS (
    SELECT
        max(v.value_text) FILTER (WHERE v.key = 'image') AS image,
        CAST(max(v.value_int) FILTER (WHERE v.key = 'generation') AS BIGINT) AS generation,
        max(v.value_bool) FILTER (WHERE v.key = 'full') AS whole,
        CAST(max(v.value_int) FILTER (WHERE v.key = 'addr') AS BIGINT) AS addr,
        CAST(max(v.value_int) FILTER (WHERE v.key = 'size') AS BIGINT) AS size,
        CAST(max(v.value_int) FILTER (WHERE v.key = 'capacity') AS BIGINT) AS capacity,
        max(v.value_text) FILTER (WHERE v.key = 'section') AS section,
        max(v.value_text) FILTER (WHERE v.key = 'class') AS class
    FROM events AS e
    JOIN event_values AS v ON v.process = e.process AND v.event_id = e.event_id
    WHERE e.build_id = $build AND e.message = 'link region'
    GROUP BY e.process, e.event_id
)
SELECT
    image,
    generation,
    whole,
    section,
    class,
    addr,
    size,
    capacity,
    addr - lag(addr + greatest(size, capacity)) OVER (PARTITION BY image, generation ORDER BY addr) AS unused_before
FROM regions
ORDER BY image, generation, addr

-- name: link_code
-- says: the code of each image a build linked (its code, patch, stub and executable named regions), by backend (stencil, cranelift, llvm, native, linker, unsaid) and by crate (the 16 largest, then the rest as one row): pieces and bytes, as the stage that laid it out said it (`link code` events, crates/blaze-link/src/shape.rs); an incremental stage's rows count only the code it placed
WITH code AS (
    SELECT
        max(v.value_text) FILTER (WHERE v.key = 'image') AS image,
        CAST(max(v.value_int) FILTER (WHERE v.key = 'generation') AS BIGINT) AS generation,
        max(v.value_bool) FILTER (WHERE v.key = 'full') AS whole,
        max(v.value_text) FILTER (WHERE v.key = 'by') AS summed_by,
        CAST(max(v.value_int) FILTER (WHERE v.key = 'pieces') AS BIGINT) AS pieces,
        CAST(max(v.value_int) FILTER (WHERE v.key = 'bytes') AS BIGINT) AS code_bytes,
        max(v.value_text) FILTER (WHERE v.key = 'what') AS what
    FROM events AS e
    JOIN event_values AS v ON v.process = e.process AND v.event_id = e.event_id
    WHERE e.build_id = $build AND e.message = 'link code'
    GROUP BY e.process, e.event_id
)
SELECT image, generation, whole, summed_by, what, pieces, code_bytes
FROM code
ORDER BY image, generation, summed_by, code_bytes DESC, what

-- name: problems
-- says: what blaze itself said was wrong, newest build first, each thing once with how often (`count`) and when in its build (`first_ms`, `last_ms` from the build's start, past its end for what was said after its answer): every event at WARN or ERROR (each line the daemon writes to its log is one), by callsite with its last message and fields; each panic and fatal signal; each `blaze-bug` and `daemon-panic` error; and a build that failed, stalled or crashed. An event of no build is the last build's to start before it. `build`: one build; unset, the last `builds` (5) builds
WITH picked AS (
    SELECT build_id AS picked_build, started_ns AS t0
    FROM builds
    WHERE CASE WHEN $build IS NULL THEN build_id > (SELECT max(build_id) FROM builds) - $builds ELSE build_id = $build END
), said AS (
    SELECT build_id AS tagged, t_ns AS said_ns, CAST(level AS VARCHAR) AS severity,
        CAST(target AS VARCHAR) || ' ' || CAST(name AS VARCHAR) AS source,
        coalesce(message, '') || coalesce(' [' || fields || ']', '') AS said_what
    FROM events_wide
    WHERE level IN ('ERROR', 'WARN')
    UNION ALL
    SELECT build_id, t_ns, CASE WHEN kind = 'signal' THEN 'FATAL' ELSE 'PANIC' END,
        CASE WHEN kind = 'signal' THEN 'signal ' || coalesce(CAST(signal AS VARCHAR), '?') ELSE 'panic at ' || coalesce(location, '?') END,
        coalesce(message, '')
    FROM panics
    UNION ALL
    SELECT build_id, t_ns, 'BUG', CAST(kind AS VARCHAR) || coalesce(' ' || function, ''), message || coalesce(': ' || detail, '')
    FROM errors
    WHERE kind IN ('blaze-bug', 'daemon-panic')
    UNION ALL
    SELECT build_id, started_ns + coalesce(dur_ns, 0), 'BUILD', 'outcome', 'the build ' || CAST(outcome AS VARCHAR) || ': `' || command || '`'
    FROM builds
    WHERE outcome IN ('failed', 'stalled', 'crashed')
), tied AS (
    SELECT coalesce(said.tagged, earlier.earlier_build) AS of_build, said.said_ns, said.severity, said.source, said.said_what
    FROM said ASOF LEFT JOIN (SELECT build_id AS earlier_build, started_ns AS earlier_ns FROM builds) AS earlier
        ON said.said_ns >= earlier.earlier_ns
)
SELECT
    tied.of_build AS build_id,
    tied.severity AS level,
    tied.source,
    arg_max(tied.said_what, coalesce(tied.said_ns, 0)) AS what,
    count(*) AS count,
    round((min(tied.said_ns) - picked.t0) / 1e6, 1) AS first_ms,
    round((max(tied.said_ns) - picked.t0) / 1e6, 1) AS last_ms
FROM tied
JOIN picked ON picked.picked_build = tied.of_build
GROUP BY tied.of_build, tied.severity, tied.source, picked.t0
ORDER BY tied.of_build DESC,
    CASE tied.severity WHEN 'FATAL' THEN 0 WHEN 'PANIC' THEN 1 WHEN 'BUG' THEN 2 WHEN 'BUILD' THEN 3 WHEN 'ERROR' THEN 4 ELSE 5 END,
    count(*) DESC, tied.source
LIMIT $limit

-- name: wait_owner
-- says: a build's `wait` spans of `min_ms` (20) or longer, longest first, each with who ended it and what that owner was doing meanwhile: `awaits`, how long (`ms`), the waiter's thread and when the wait began from the build's start (`at_ms`), `owned_by` (job, the scope's jobs, whoever sends, the kernel, the lock's holder, the daemon, not said), the `owner` job, and for a job's the spans of that job that overlapped the wait, the most overlap first (`owner_did`: each span's name and what it was of with the milliseconds of the wait it covered, so a wait behind a query's run shows the run and the queries under it, and a wait behind a verify shows no run of the awaited key at all) with the owner's thread; `owner_did` null with a job owner means that job has no span in the wait's window (it was parked or queued itself: see `critical_path`)
WITH bounds AS (
    SELECT min(started_ns) AS t0 FROM builds WHERE build_id = $build
), waits AS (
    SELECT process, span_id, CAST(thread_name AS VARCHAR) AS thread, awaits, owner,
        CAST(owned_by AS VARCHAR) AS owned_by, CAST(waits_on AS VARCHAR) AS waits_on,
        start_ns, start_ns + dur_ns AS end_ns, dur_ns
    FROM spans
    WHERE build_id = $build AND name = 'wait' AND dur_ns >= $min_ms * 1e6
), runs AS (
    SELECT process, thread, t_ns, CASE WHEN event = 'started' OR event = 'resumed' THEN job END AS on_job
    FROM switches
), laned AS (
    SELECT s.process, CAST(s.thread_name AS VARCHAR) AS thread, CAST(s.name AS VARCHAR) AS name, s.function, s.unit,
        s.start_ns, s.start_ns + s.dur_ns AS end_ns, runs.on_job
    FROM spans AS s ASOF LEFT JOIN runs
        ON s.process = runs.process AND s.thread = runs.thread AND s.start_ns >= runs.t_ns
    WHERE s.name NOT IN ('wait', 'job', 'worker')
), covered AS (
    SELECT waits.process, waits.span_id, laned.thread, laned.name, coalesce(laned.function, laned.unit) AS what,
        sum(least(laned.end_ns, waits.end_ns) - greatest(laned.start_ns, waits.start_ns)) AS ns
    FROM waits JOIN laned
        ON laned.process = waits.process AND laned.on_job = waits.owner
        AND laned.start_ns < waits.end_ns AND laned.end_ns > waits.start_ns
    GROUP BY ALL
), did AS (
    SELECT process, span_id, any_value(thread) AS owner_thread,
        string_agg(name || coalesce(' ' || what, '') || ' ' || CAST(round(ns / 1e6, 1) AS VARCHAR) || ' ms', '; ' ORDER BY ns DESC, name, what) AS owner_did
    FROM (SELECT *, row_number() OVER (PARTITION BY process, span_id ORDER BY ns DESC, name, what) AS rank FROM covered)
    WHERE rank <= 8
    GROUP BY process, span_id
)
SELECT
    round(waits.dur_ns / 1e6, 3) AS ms,
    waits.awaits,
    waits.waits_on,
    waits.thread,
    round((waits.start_ns - bounds.t0) / 1e6, 3) AS at_ms,
    coalesce(waits.owned_by, CASE WHEN waits.owner IS NULL THEN 'not recorded (a blaze before owned_by)' ELSE 'job' END) AS owned_by,
    waits.owner,
    did.owner_thread,
    did.owner_did
FROM waits CROSS JOIN bounds
LEFT JOIN did ON did.process = waits.process AND did.span_id = waits.span_id
ORDER BY waits.dur_ns DESC, waits.start_ns
LIMIT $limit

-- name: timeline
-- says: a build as rows in the order things began: each span of `min_ms` (1) or longer with when it began from the build's start (`at_ms`), how long it was open (`ms`), its own CPU (`cpu_ms`, less the spans under it), its thread, its depth on that thread, and what it was of (its function, what a wait awaited, its unit, its crate, or its other fields); spans of no build in the build's window are its too. A gap between one row's end and the next row's `at_ms` on the same thread and depth is time under no span of that size. The path across waits to the jobs that ended them is `critical_path`
WITH bounds AS (
    SELECT started_ns AS t0, started_ns + dur_ns AS t1
    FROM builds
    WHERE build_id = $build
)
SELECT
    round((s.start_ns - bounds.t0) / 1e6, 3) AS at_ms,
    round(s.dur_ns / 1e6, 3) AS ms,
    round(s.self_cpu_ns / 1e6, 3) AS cpu_ms,
    CAST(s.thread_name AS VARCHAR) AS thread,
    s.depth,
    CAST(s.name AS VARCHAR) AS name,
    coalesce(s.function, s.awaits, s.unit, CAST(s.crate AS VARCHAR), s.fields) AS what
FROM spans AS s, bounds
WHERE (s.build_id = $build OR (s.build_id IS NULL AND s.start_ns >= bounds.t0 AND s.start_ns < bounds.t1))
    AND s.dur_ns >= $min_ms * 1e6
ORDER BY s.start_ns, s.depth
LIMIT $limit

-- name: long_spans
-- says: a build's spans of `min_ms` (20) or longer, longest first: how long (`ms`), its own CPU (`cpu_ms`), its thread, its name, what it was of, and when it began from the build's start (`at_ms`); a long span with little CPU that is no `wait` blocked in something no span names
SELECT
    round(s.dur_ns / 1e6, 3) AS ms,
    round(s.self_cpu_ns / 1e6, 3) AS cpu_ms,
    CAST(s.thread_name AS VARCHAR) AS thread,
    CAST(s.name AS VARCHAR) AS name,
    coalesce(s.function, s.awaits, s.unit, CAST(s.crate AS VARCHAR), s.fields) AS what,
    round((s.start_ns - (SELECT min(started_ns) FROM builds WHERE build_id = $build)) / 1e6, 3) AS at_ms
FROM spans AS s
WHERE s.build_id = $build AND s.dur_ns >= $min_ms * 1e6
ORDER BY s.dur_ns DESC, s.start_ns
LIMIT $limit

-- name: waits_by_reason
-- says: a build's waits by what they waited for (`wait` spans by `awaits`, `admission` spans by what held the task back): how many, their time summed over the threads waiting (not wall time), the longest, a thread that waited, and who ends such a wait (`owned_by`; what its owner did meanwhile is `wait_owner`); waits too short for a row (`folded`) are not counted
SELECT
    CAST(s.name AS VARCHAR) AS waited_in,
    coalesce(s.awaits, '(unnamed)') AS reason,
    count(*) AS waits,
    round(sum(s.dur_ns) / 1e6, 3) AS ms,
    round(max(s.dur_ns) / 1e6, 3) AS longest_ms,
    CAST(any_value(s.thread_name) AS VARCHAR) AS a_thread,
    CAST(any_value(s.owned_by) AS VARCHAR) AS owned_by
FROM spans AS s
WHERE s.build_id = $build AND s.name IN ('wait', 'admission')
GROUP BY s.name, s.awaits
ORDER BY sum(s.dur_ns) DESC, reason
LIMIT $limit

-- name: counts_by_cause
-- says: what the query engine counted in a build, per query that ran or input that was written: its runs and, of them, first runs, runs for a changed read, for an untracked read and for a cycle, runs for a value evicted and asked again, runs in a view it had already run in, rows verified without running, and an input's writes; the most run first. An edit's build should run a handful
SELECT query, executed, ran_new, ran_changed, ran_untracked, ran_cycle, evicted_asked, reran, verified, written
FROM query_counts
WHERE build_id = $build AND (executed > 0 OR written > 0)
ORDER BY executed DESC, written DESC NULLS LAST, query
LIMIT $limit

-- name: writes
-- says: the inputs a build wrote, by kind with names: each input a setter wrote (`query_counts.written`, an equal value too), and beside it each of its fields a rerun names as its cause (`provenance.cause_key`: which file, which crate, which module's scope) with how many memos ran again for it; an input written that made nothing run has one row with no field
WITH wrote AS (
    SELECT CAST(query AS VARCHAR) AS input, written
    FROM query_counts
    WHERE build_id = $build AND written > 0
), caused AS (
    SELECT cause_key, count(DISTINCT query_key) AS memos
    FROM provenance
    WHERE build_id = $build AND outcome = 'ran' AND cause_kind IN ('input', 'tracked')
    GROUP BY cause_key
)
SELECT wrote.input, wrote.written, caused.cause_key AS field, caused.memos
FROM wrote
LEFT JOIN caused ON starts_with(caused.cause_key, '`' || wrote.input || '.')
ORDER BY caused.memos DESC NULLS LAST, wrote.written DESC, wrote.input, caused.cause_key
LIMIT $limit

-- name: scripts
-- says: the build scripts a build ran, slowest first: the unit, how long its process lived, how long making its sandbox took, its CPU, its exit code, and why it ran instead of being reused (`misses` of step `build script run`: the cause and what changed). A script whose kept run was reused has no row: it ran no process
SELECT
    p.unit,
    round((p.exited_ns - p.spawned_ns) / 1e6, 1) AS ms,
    round(p.sandbox_ns / 1e6, 1) AS sandbox_ms,
    round((p.user_ns + p.system_ns) / 1e6, 1) AS cpu_ms,
    p.code,
    CAST(m.cause AS VARCHAR) AS cause,
    m.what
FROM processes AS p
LEFT JOIN misses AS m
    ON m.build_id = p.build_id AND m.step = 'build script run'
    AND (starts_with(p.unit, m.item) OR starts_with(m.item, p.unit))
WHERE p.build_id = $build AND p.kind = 'build_script'
ORDER BY p.exited_ns - p.spawned_ns DESC, p.unit
LIMIT $limit

-- name: pool
-- says: the pools' counters over a build: the store's buffer pool (`pool resident bytes`, `pool pinned bytes`, `pool loaded bytes`, `pool prefetched bytes`, `pool evicted bytes`, `pool pinned bound waits`) and the worker pool (`pool queued`, `pool running`), each with its value as the build began and ended, its most, and how many readings; a counter that counts up (loaded, evicted, waits) grew by end less start
WITH bounds AS (
    SELECT started_ns AS t0, started_ns + dur_ns AS t1
    FROM builds
    WHERE build_id = $build
)
SELECT
    CAST(c.name AS VARCHAR) AS counter,
    CAST(arg_min(c.value, c.t_ns) AS BIGINT) AS at_start,
    CAST(arg_max(c.value, c.t_ns) AS BIGINT) AS at_end,
    CAST(max(c.value) AS BIGINT) AS most,
    count(*) AS readings
FROM counters AS c, bounds
WHERE starts_with(CAST(c.name AS VARCHAR), 'pool ')
    AND (c.build_id = $build OR (c.build_id IS NULL AND c.t_ns >= bounds.t0 AND c.t_ns <= bounds.t1))
GROUP BY c.name
ORDER BY counter

-- name: kept
-- says: what a build kept or lost as it ended and after its answer was sent, to the next build's start: each span about keeping (the front end kept or let go, expansions published, what the store was given, link images evicted), in order, then whether what it left was lost (`misses` of step `results kept after the answer`: `the build before` lost what this build then computed again, `this build` lost what the next one did)
WITH bounds AS (
    SELECT started_ns AS t0,
        coalesce((SELECT min(started_ns) FROM builds WHERE build_id > $build), started_ns + dur_ns + 60000000000) AS t1
    FROM builds
    WHERE build_id = $build
)
SELECT
    round((s.start_ns - bounds.t0) / 1e6, 3) AS at_ms,
    round(s.dur_ns / 1e6, 3) AS ms,
    CAST(s.thread_name AS VARCHAR) AS thread,
    CAST(s.name AS VARCHAR) AS name,
    coalesce(s.fields, s.function, s.unit, CAST(s.crate AS VARCHAR)) AS what
FROM spans AS s, bounds
WHERE s.start_ns >= bounds.t0 AND s.start_ns < bounds.t1
    AND (s.name LIKE '%kept%' OR s.name LIKE '%let go%' OR s.name LIKE 'store %' OR s.name LIKE '%settl%')
UNION ALL
SELECT
    NULL,
    NULL,
    CASE WHEN m.build_id = $build THEN 'the build before' ELSE 'this build' END,
    'LOST: ' || CAST(m.step AS VARCHAR),
    m.what
FROM misses AS m
WHERE m.step = 'results kept after the answer' AND m.build_id IN ($build, $build + 1)
ORDER BY at_ms NULLS LAST, name
LIMIT $limit

-- name: syscalls
-- says: what a build's threads blocked in, as the daemon itself saw it, with no strace: each file call a driver made off its loop (`io call`: a `wait` span whose `awaits` is `io <op> <file>`: a clone, a write-back, a listing, with how long it took and the span it was made in), each step of the daemon's loop that ran in line past its bound (`loop step in line`: the step and its time), and the samples of threads blocked inside a span (`blocked samples`: the stack, outermost first, the span, how many, and the time they stand for at 49 a second), in the order they began. `thread`: a thread's name (`LIKE`), or `loop` for the daemon's loop thread; `within`: text the call or its span holds (`LIKE`); `min_ms`: the least time. A system call no driver times and no sample caught is not here: harness/v2/loop_syscalls.py (strace) sees those
WITH bounds AS (
    SELECT started_ns AS t0, started_ns + dur_ns AS t1
    FROM builds
    WHERE build_id = $build
), looped AS (
    SELECT DISTINCT process AS loop_process, thread AS loop_thread
    FROM spans
    WHERE build_id = $build
        AND (name = 'request: read to reply' OR name = 'loop stalled' OR starts_with(CAST(name AS VARCHAR), 'loop: '))
), named AS (
    SELECT process AS named_process, thread AS named_thread, CAST(any_value(thread_name) AS VARCHAR) AS said_name
    FROM spans
    WHERE build_id = $build
    GROUP BY process, thread
), calls AS (
    SELECT s.process AS call_process, s.thread AS call_thread, CAST(s.thread_name AS VARCHAR) AS on_thread,
        s.start_ns AS began_ns, s.dur_ns AS took_ns, 'io call' AS seen_as, s.awaits AS call,
        CAST(p.name AS VARCHAR) || coalesce(' ' || p.function, '') AS inside, CAST(1 AS BIGINT) AS times
    FROM spans AS s
    LEFT JOIN spans AS p ON p.process = s.process AND p.span_id = s.parent
    CROSS JOIN bounds
    WHERE s.name = 'wait' AND starts_with(s.awaits, 'io ') AND s.start_ns >= bounds.t0 AND s.start_ns < bounds.t1
    UNION ALL
    SELECT s.process, s.thread, CAST(s.thread_name AS VARCHAR), s.start_ns, NULL, 'loop step in line', s.function, NULL, CAST(1 AS BIGINT)
    FROM spans AS s
    CROSS JOIN bounds
    WHERE s.name = 'loop stalled' AND s.start_ns >= bounds.t0 AND s.start_ns < bounds.t1 + 1000000000
    UNION ALL
    SELECT m.process, m.thread, any_value(named.said_name), min(m.t_ns), CAST(count(*) * 20408163 AS BIGINT), 'blocked samples',
        array_to_string(m.stack, ' > '), CAST(p.name AS VARCHAR) || coalesce(' ' || p.function, ''), CAST(count(*) AS BIGINT)
    FROM samples AS m
    LEFT JOIN spans AS p ON p.process = m.process AND p.span_id = m.span_id
    LEFT JOIN named ON named.named_process = m.process AND named.named_thread = m.thread
    CROSS JOIN bounds
    WHERE m.cpu = 'off' AND m.t_ns >= bounds.t0 AND m.t_ns < bounds.t1
    GROUP BY m.process, m.thread, m.span_id, array_to_string(m.stack, ' > '), p.name, p.function
)
SELECT
    round((calls.began_ns - bounds.t0) / 1e6, 3) AS at_ms,
    round(calls.took_ns / 1e6, 3) AS ms,
    calls.on_thread AS thread,
    calls.seen_as,
    calls.call,
    calls.inside,
    calls.times
FROM calls
CROSS JOIN bounds
WHERE (calls.on_thread LIKE $thread
        OR ($thread = 'loop' AND EXISTS (
            SELECT 1 FROM looped WHERE looped.loop_process = calls.call_process AND looped.loop_thread = calls.call_thread)))
    AND (coalesce(calls.inside, '') LIKE $within OR coalesce(calls.call, '') LIKE $within)
    AND (calls.took_ns IS NULL OR calls.took_ns >= $min_ms * 1e6)
ORDER BY calls.began_ns, calls.seen_as
LIMIT $limit

-- name: unit_reads
-- says: why a unit whose record key is as it was (`misses.cause` `beyond`) was made again: each read its record is filed by past its key (a process variable `env!` read, a file `include!` read outside every source root, what a proc-macro expansion read) whose value when the build looked its record up (`looked`) is not the value the last build that kept the record filed it under (`filed`, in build `filed_in`); a read with no `filed` was in no record kept before, and one unit with every read listed and none filed had no record kept at all (`misses` of step `unit record` says why). Events `a unit record's read` (blaze-cargo's `serve/unit/compile.rs`, `say_reads`)
WITH said AS (
    SELECT
        e.build_id AS said_build,
        max(v.value_text) FILTER (WHERE v.key = 'unit') AS unit,
        max(v.value_text) FILTER (WHERE v.key = 'when') AS said_when,
        max(v.value_text) FILTER (WHERE v.key = 'read') AS read,
        max(v.value_text) FILTER (WHERE v.key = 'value') AS read_value
    FROM events AS e
    JOIN event_values AS v ON v.process = e.process AND v.event_id = e.event_id
    WHERE e.build_id <= $build AND e.message = 'a unit record''s read'
    GROUP BY e.build_id, e.process, e.event_id
), looked AS (
    SELECT DISTINCT unit, read, read_value FROM said WHERE said_when = 'looked' AND said_build = $build
), filed AS (
    SELECT unit, read, read_value, said_build,
        row_number() OVER (PARTITION BY unit, read ORDER BY said_build DESC) AS nth
    FROM said
    WHERE said_when = 'filed' AND said_build < $build
)
SELECT looked.unit, looked.read, filed.read_value AS filed, filed.said_build AS filed_in, looked.read_value AS looked
FROM looked
LEFT JOIN filed ON filed.unit = looked.unit AND filed.read = looked.read AND filed.nth = 1
WHERE filed.read_value IS DISTINCT FROM looked.read_value
ORDER BY looked.unit, looked.read
LIMIT $limit

-- name: edit_work
-- says: the work a build did, by stage, as each stage counted it: how many things it visited, asked of or made again, which after an edit should be the edit's size and not the program's (rule incremental-first). `engine memos` are the query engine's rows run (`executed`) and proved unchanged without running (`verified`), from `query_counts`; `changed set` is the engine's walk from what was written (`visited` rows verified, `changed` the rows whose value moved); `body check` is each unit's plan (`visited_bodies` and `visited_modules` got a task, `held_*` is the crate, `whole` counts plans that visited everything); `codegen walk` is each program's walk (`asked` functions asked of again, `described` made again, `replaced` rows of the kept lists, `whole` walks from the roots); `dependency image keys` are the keys a serve asked of the database (`asked`), those that moved (`moved`) and those made again (`computed`); `link` is each link's rows made again (`rows`) and the names and patchable functions it went over (`symbols`, `patchable`, the part that still follows the program). `total` sums a counter over the build, `times` is how many units or walks said it; a counter nothing said has no row. Events `edit work` (crates/blaze-query/src/db/edit_work.rs names the stages)
WITH said AS (
    SELECT
        e.process AS said_process,
        e.event_id AS said_event,
        max(v.value_text) FILTER (WHERE v.key = 'stage') AS stage
    FROM events AS e
    JOIN event_values AS v ON v.process = e.process AND v.event_id = e.event_id
    WHERE e.build_id = $build AND e.message = 'edit work'
    GROUP BY e.process, e.event_id
), counted AS (
    SELECT said.stage AS stage, v.key AS counter, CAST(v.value_int AS BIGINT) AS n
    FROM said
    JOIN event_values AS v ON v.process = said.said_process AND v.event_id = said.said_event
    WHERE v.value_int IS NOT NULL
    UNION ALL
    SELECT 'engine memos' AS stage, 'executed' AS counter, CAST(sum(q.executed) AS BIGINT) AS n
    FROM query_counts AS q
    WHERE q.build_id = $build
    UNION ALL
    SELECT 'engine memos' AS stage, 'verified' AS counter, CAST(sum(q.verified) AS BIGINT) AS n
    FROM query_counts AS q
    WHERE q.build_id = $build
    UNION ALL
    SELECT 'dependency image keys' AS stage, 'computed' AS counter, CAST(sum(q.executed) AS BIGINT) AS n
    FROM query_counts AS q
    WHERE q.build_id = $build AND ends_with(CAST(q.query AS VARCHAR), 'crate_image_key')
)
SELECT stage, counter, CAST(sum(n) AS BIGINT) AS total, count(*) AS times
FROM counted
WHERE n IS NOT NULL
GROUP BY stage, counter
ORDER BY stage, counter
LIMIT $limit
