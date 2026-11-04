# Handoff: the flight recorder reads thread CPU at boundaries, not once a span (lane pd-cost)

Worktree /Volumes/Projects/blaze-wt-pd-cost, branch pd-cost. Not committed. **Nothing was built, checked or
tested**, per the brief. Expect some compile fixes on the first `blaze check`.

## What changed, in short

Before, every span made two `CLOCK_THREAD_CPUTIME_ID` syscalls (open and close, about 210 ns) and
wrote a second 64 B `cols` row whenever its self CPU was above 0. Now:

- Thread CPU is read only at **boundaries**: fiber switches (all but `ready`), thread park and
  unpark (`take_open` and `put_open`), and a few rare cuts (listed below).
- A span adds the cycle-counter ticks it spent as the innermost entered span to its slot (`pending`).
- At a boundary, those ticks become CPU at the stretch's rate (CPU over wall ticks).
- A span closing inside its stretch, which is most of them, carries its ticks as a 24-bit share of its
  duration in the top bits of its row's alloc-bytes word. The reader resolves it at the ring's next
  CPU row. No extra row.
- `switches` has a new `cpu_ns` column.

## Files changed

- `crates/blaze-profile/src/time/cpu.rs` (new):
  - the module docs hold the design, the semantics and the error bound with its proof;
  - `AGE` 250 µs, `ROOT_AGE` 20 µs, `GAP` 10 µs, `SLIVER` 2 µs;
  - `Reading`, `Rate` (`between`, `parts`, `cpu_of`), and the share encoding (`SHARE_BITS`,
    `SHARE_COUNTED`, `SHARE_FIRST`, `SHARE_ONE`, `share_word`, `Share`, `share_of`);
  - hegel tests.
- `crates/blaze-profile/src/flight.rs`:
  - the recorder, described below;
  - `Plot` gains `ProfileCostNs`, `ProfileSpansMade` and `ProfileCpuReads`; `Plot::ALL` is now `[Plot; 24]`;
  - new `pub fn cost() -> Cost`, `pub struct Cost`, `pub const COST_SAMPLE` and `pub const HOLD_MAX`.
- `crates/blaze-profile/src/time.rs`:
  - `pub mod cpu`;
  - `SwitchIn.cpu_ns: Option<i64>`;
  - the `SpanIn.self_cpu_ns` doc.
- `crates/blaze-profile/src/time/load.rs`:
  - the switches SQL selects `cpu_ns`;
  - the `SWITCHES_SINCE_2` fallback gives `NULL::BIGINT AS cpu_ns`.
- `crates/blaze-profile/src/schema.rs`:
  - `switches.cpu_ns` (I64, nullable) is appended last, and VERSION stays 3, since an appended
    nullable column doesn't bump it;
  - the `spans.self_cpu_ns` doc is rewritten.
- `crates/blaze-profile/src/batches.rs`: `switches()` writes the `cpu_ns` column.
- `crates/blaze-profile/src/history.rs`: `flush()` writes the counters `profile cost ns`,
  `profile spans made` and `profile cpu reads` from `flight::cost()`, with each build's flush only.
- `crates/blaze-profile/src/slow.rs`: `Recorder` gains `cost_ns`, `spans_made` and `cpu_reads`.
- `crates/blaze-profile/src/slow/load.rs`: `recorder_sql` reads three more deltas, and `recorder()`
  reads them.
- `crates/blaze-profile/src/slow/render.rs`:
  - `recorder()` appends `; cost C (X ns a span over N spans, R thread-CPU reads)`;
  - new `recorder_cost()`.
- `crates/blaze-bench/Cargo.toml` and `Cargo.lock`:
  - dev-deps `blaze-profile` and `tracing` (lock edited by hand);
  - `[[bench]] record`.
- `crates/blaze-bench/benches/record.rs` (new): `record/root` and `record/nested`, with a budget of
  30 ns a span each.
- `docs/v2/plan/lanes/observe/speed-of-light.md`:
  - status `plan` → `current`;
  - corrects CAP to 32768 rows (2 MiB) and the open slots to `SLOTS_OPEN` 16384 in `CHUNK`s of 256 made on demand;
  - new "Self CPU" section;
  - the "How to measure" section names the bench and the `blaze slow` line.
- `docs/profiling.md`:
  - `spans.self_cpu_ns` text;
  - the `switches.cpu_ns` row;
  - the three new counters;
  - the example `recorder:` line.

## flight.rs: exact functions changed

- **Removed:** `JOB_CPU`, `CPU_CLOCKS`, `job_cpu`, `held_cpu`, `self_cpu`, and `Carried.cpu_clock` and
  `Carried.cpu_at`.
- **New:**
  - statics: `counting`, `CPU_AGE_TICKS`, `CPU_ROOT_AGE_TICKS`, `CPU_GAP_TICKS`, `CPU_SLIVER_TICKS`;
  - constants: `UNCOUNTED_CPU`, `NO_CPU`, `LEFT_MAX`, `CPU_ROW`, `CPU_VALID`, `CPU_READ`, `SHARE_SHIFT`, `BYTES_MAX`;
  - `enum CpuWhy`;
  - functions: `cost_from`, `cost_to`, `stretch_age`, `accrue`, `counted_from`, `resolve`, `boundary`,
    `boundary_in`, `cpu_leave`, `left_open`, `cost`, `settle_folds`, `put_fold_settled`, `is_cpu_row`,
    `waits_for_rate`;
  - `enum Taking`.
- **Changed:**
  - `count_cpu`: same signature; it also sets the tick thresholds.
  - `mark`, `take_open`, `put_open` and `switched` each add a boundary or set the stretch.
  - `began`, `entered`, `exited`, `closed`: rewritten CPU parts and cost sampling.
  - `claim` and `Held::drop`: reset the CPU state and run a boundary.
  - `folded`: new signature `(ring, opened, parent_site, now, cpu, pending, allocs, bytes)`.
  - `put_fold` and `put_folds`.
  - `read` and `window` now call `take(into, Taking)`; `take` is rewritten.
- **Struct fields:**
  - `Slot`: `cpu_clock`, `cpu`, `cpu_under` and `cpu_entered` are replaced by `cpu` (known ns, or
    `UNCOUNTED_CPU`) and `pending` (ticks).
  - `Opened`: just `cpu`.
  - `Ring` gains `top_since`, `stretch_ticks`, `stretch_cpu`, `rate_ticks`, `rate_cpu`, `idle_since`,
    `left[16]`, `left_len`, `cost`, `cpu_reads` and `sample`.
  - `Fold` gains `pending` and `uncounted`.
  - `Folds` gains `dirty: u128`.
  - `SwitchRow.cpu_ns`.
  - `Read.waiting` (scratch).

## Row layouts changed

- **Span row, word 7:** bits 0..40 hold alloc bytes, saturating at `BYTES_MAX` (about 1 TiB). Bits
  40..64 hold the CPU share word:
  - 0: not counted;
  - 1: counted, no ticks waiting;
  - 2 and up: the ticks waiting, as a fraction (`SHARE_ONE` parts) of the span's duration.
- **New CPU row** (plot byte `0xf8`, KIND_PLOT): `[at_ticks, cpu_ns, from_ticks, from_cpu_ns,
  KIND_PLOT<<62 | CPU_READ(1<<17) | CPU_VALID(1<<16) | why<<8 | 0xf8, 0, build, 0]`.
  - One is written per boundary.
  - Right after a switch row (why = Switch), it gives that switch its `cpu_ns`.
- **`cols` row with `CPU_COL`:** now written only when the span outlived a stretch (known CPU > 0),
  not for every span with CPU.
- **Fold rows:** unchanged layout. A sum whose ticks never got a rate says not counted.

## Design

- **Semantics.** `self_cpu_ns` is the CPU while the span was the innermost entered span on its
  thread. This is the old value for every span that is entered while it works (kinds, `.entered()`,
  `in_scope`, held spans). **One change:** a span never entered now counts 0; it used to get its
  job's CPU while open. This matches tracing's busy time and how held spans already worked.
- **Boundaries** (one CPU read each, `boundary_in`):
  - every switch but `ready`;
  - a wait span's open and close, when the stretch is at least `SLIVER` old;
  - the first root after `GAP` in no span;
  - a root's close when the stretch is at least `ROOT_AGE` old;
  - any close at least `AGE` into a stretch;
  - `claim`, `Held::drop`, `mark` and a read on the reading thread;
  - a full `left` list;
  - a fold with waiting ticks being written;
  - `take_open` when the stretch isn't fresh.

  At each one: write the CPU row, resolve `pending` for the stack, the `left` list and dirty folds,
  and store the last rate.
- **Park.** In `cpu_leave`, if a switch boundary happened within `SLIVER`, the sliver since takes
  that stretch's rate, so there is no second read. Otherwise it cuts.
- **Reader.**
  - A waiting span row resolves at its ring's next CPU row.
  - With a cursor, the rows from the first waiting span after the last CPU row are held back to the
    next read, unless that span closed more than `HOLD_MAX` (1 s) ago.
  - `window` and leftovers give `None`.
- **Cost.** 1 in 64 calls of `began`, `entered`, `exited` and `closed`, chosen at random
  (xorshift), is timed and counted ×64. Every boundary is timed fully. `cost()` sums over rings.

## Error bound

|derived − per-span reads| ≤ Σ over stretches s of min(L_s, O_s) + 1 tick for each enter and exit
+ 1 ns for each stretch + dur / 2^23.

- L_s is the span's time in stretch s; O_s is the off-CPU time in s.
- The 1 ns is from rounding down.
- dur / 2^23 is the 24-bit share.

Stretches are short and blaze's workers are rarely preempted, so the first term is near 0. Two
exceptions, each bounded by that span's waiting ticks:

- a span exited on one thread and closed on another takes the closing thread's rate;
- the park sliver takes the last rate.

Proof and test: `time/cpu.rs`.

## Risks

- Held-back rows land in a later flush file (they keep their `build_id`). A build's final flush can
  miss a busy thread's last spans until the next flush.
- A ring silent past `HOLD_MAX` with waiting spans reads them as `None` (not 0).
- Spans that began before `count_cpu`, or before their thread's first boundary, read as `None`.
- `entered` and `exited` each read ticks when counting, two cheap counter reads a span.
- `Plot::ALL` grew to 24: anything indexing plot bytes by count is affected. The flight test that
  asserts row bytes are at least `Plot::ALL.len()` still holds (`0xf8` > 24).
- The cost counters are written with each build's flush only. A build's delta includes the idle time
  since the previous build's flush.
- The legacy `BLAZE_PROFILE` path (`lib.rs` `profile_began`) still calls `thread_cpu_ns` per span. I
  left it alone.
- A bug fixed in review: `accrue` now advances `top_since`, so a boundary right after an `exited`
  accrual doesn't double-count.

## Tests (none run)

- `time/cpu.rs` (hegel):
  - `a_share_reads_back_within_a_part_of_its_spans_duration`;
  - `a_stretchs_rate_gives_its_whole_cpu_to_its_whole_wall`;
  - `self_cpu_from_stretches_is_the_per_span_reads_within_the_bound`: a synthetic timeline with
    random CPU, enter/exit and boundaries, checking derived against exact within the bound;
  - `a_thread_always_on_cpu_gets_each_spans_cpu_to_the_roundings`.
- `flight.rs`:
  - `a_spans_self_cpu_is_its_own_while_innermost_less_the_spans_under_it_and_what_ran_while_it_parked`
    replaces the old CPU test. It uses busy-waits and fake CPU, with tolerance want/20 + 300 µs.
  - `a_span_read_before_its_threads_next_boundary_waits_for_it_and_is_never_read_uncounted`: the
    cursor hold-back.

## Bench

`crates/blaze-bench/benches/record.rs`:

- `record/root`: a root span opened, entered, exited and closed.
- `record/nested`: the same under a held outer span.

Both run with `count_cpu` on, as in production. Budget: 30 ns a span. CI's bench.yaml still builds
only `store`, so add `record` there if wanted.

## What to measure

1. `blaze-bench record` on this tree against main: ns a span before and after. Expect about 230 → about 25 ns on the Mac.
2. `blaze slow` on a self-build: the `recorder:` line's `cost … ns a span` and the thread-CPU reads.
   Reads a second should track switches, not spans.
3. gate and `blaze slow` on a profiled self-build still pass. Compare the per-thread sum of
   `self_cpu_ns` (plus folded) with switch-row `cpu_ns` deltas on the same thread; they should agree
   to within a few %.
4. Spans with `self_cpu_ns IS NULL` among those entered should be about 0 outside thread start-up.
5. Ring rows a span: the `cols` share should drop sharply.
