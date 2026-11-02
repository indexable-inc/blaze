---
status: plan
summary: lane proof's speed of light: the bound of the oracle's comparisons and the harness, the arithmetic, and how the design reaches 90% of it.
date: 2026-10-30
---
# Lane proof: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). The machine figures are those of
[link's speed of light](../link/speed-of-light.md#the-machine), unless a section says otherwise.
`proof` is also where every other lane's bounds are measured: its budgets are each bound times
1.1.

## The oracle and the harness: `crates/blaze-v2-proof/`, `harness/v2/` (speed-link-edge-sol-proof)

The machine is link's: 16 cores, 25 GB/s from DRAM a core and 400 GB/s a socket, BLAKE3 at
6 GB/s a core, SHA-256 at 2 GB/s. One correction to link's table: a read fault on a file in the
page cache maps 16 pages at once (Linux's fault-around, 64 KiB), so a mapped file costs about
0.25 µs per 64 KiB, not per 4 KiB page. The figures below use that. Every "now" is arithmetic on
the code as written; nothing here was run (this lane's brief ran nothing). Proof's budgets are what
turn these figures into measurements.

### First difference (`crates/blaze-v2-proof/src/bytes.rs`)

**Workload.** Two outputs that should be the same, compared byte for byte: a 512 MiB debug binary
pair (nushell's size) that differs, at worst, in its last byte. Most determinism checks compare
many small files, and those are bounded by the open and map calls (0.4 µs + 0.25 µs a file), not by
bytes.

**Bound.** 2 × 512 MiB = 1.07 GB read. On one core that is 1.07 GB ÷ 25 GB/s = 43 ms, plus faults
of 16,384 × 0.25 µs = 4 ms, so 47 ms. Across 16 cores it is 1.07 GB ÷ 400 GB/s = 2.7 ms, plus
the faults spread over 16 threads (0.25 ms), so 2.9 ms. For many files at once the bound is the
same 2 × Σ bytes ÷ 400 GB/s.

**Was.** One thread with an AVX2 or NEON loop (about 24 bytes a cycle a side, so memory-bound),
over maps made with `MAP_POPULATE`. Populating faults in both files on the calling thread before
the compare starts. The whole run is 60 to 70 ms on one core, 1.4× the one-core bound and 22× the
16-core bound.

**Changed.** `first_difference_across_cores` splits a pair of at least 64 MiB into 4 MiB blocks
over the cores. A shared atomic "earliest piece with a difference" (`fetch_min`) lets later
pieces stop early. The earliest such piece never stops, so the answer is the same as the serial
scan's. Files of that size are mapped lazily (`Mapped::open_lazily`, no populate), so each thread
faults in its own blocks. Tests compare the parallel answer with the scalar one for differences
in every block and for both length orders.

**Now.** About 3.3 to 3.6 ms on 16 cores: 2.7 ms streaming, 0.25 ms faults, and 16 thread spawns
at about 15 µs each. That is within 25% of the bound. The rest is thread start-up, which a
persistent pool would remove.

Corrected by speed-sol2-proof (2026-10-30): the slow path named here is gone.
`harness/v2/determinism.py` used `engine.first_difference` (Python `readinto` in 1 MiB pieces,
then `memcmp`, about 150 ms for the pair, 3× the one-core bound). It now calls
`engine.first_difference_by`, which runs `blaze-v2-proof '{"first": …, "second": …}'` (this code)
and reads only the two bytes at the offset to word the answer as before. That costs about 5 ms:
the 3.3 to 3.6 ms above plus one process start. `proof.py selftest PATH` holds it to the model
of the old answer for every size and kind of difference. `engine.first_difference` stays only
as `proof.py`'s model.

**State of the art.** glibc's `memcmp` (AVX2/EVEX) and `cmp(1)` are one-core scans, about 43 ms
here. The same loop across cores reaches the socket's bandwidth instead of one core's.

```bound
{"name": "proof: first difference, 512 MiB pair", "bound": 2.9, "unit": "ms", "program": "blaze-v2-proof", "args": ["{\"bench\": \"first-difference\"}"]}
```

### Hashing artifacts (`harness/v2/engine.py`)

**Workload.** The digests `engine.digests` and `tree_digest` take of a build's outputs and trees.
For nushell's debug outputs that is about 3 GB in about 1,000 files, the largest about 500 MB.
For a source tree it is about 30,000 files and 300 MB.

**Bound.** Each byte read once, hashed at full width: 3 GB ÷ (6 GB/s × 16) = 31 ms for the
outputs. For the tree it is 300 MB ÷ 96 GB/s = 3 ms, plus 30,000 opens at 0.4 µs ÷ 16 = 1 ms, so
4 ms (6 ms with the stat of each entry). SHA-256 at 2 GB/s a core is 3× slower at the same width
(94 ms). On one thread per file it is worse still: the critical path is the largest file alone,
500 MB ÷ 2 GB/s = 250 ms, 8× the bound.

**Was.** SHA-256 (`hashlib`), one file per pool task, one core per file. The critical path was
the largest file at about 250 ms, 8× off.

**Changed.**
- SHA-256 is gone from every digest that blaze or the harness picks. Digests use BLAKE3 (the
  `blake3` Python package, Rust's `blake3` crate underneath).
- A file of at least 8 MiB is hashed across cores (`max_threads=AUTO`), so the 500 MB file no
  longer sets the critical path.
- Files under 1 MiB are read with one `read()`; larger files are mapped.
- Small files go to the pool in batches (at most 256 files or 4 MiB a task), so pool overhead is
  paid per batch, not per file.
- Files are queued largest first.
- `tree_digest`'s domain moved to `blaze v2 proof tree 2`, so no old SHA-256 tree digest is
  compared with a new one.
- What stays SHA-256 is what something outside chose: the crates.io check in `corpus.verified`
  (the index lists SHA-256), and `Kept.key` (bytes, not files). The package is a request in
  `v2/requests/speed.md`: add `blake3` to `pyproject.toml` and `uv.lock`.

**Now.** For outputs, about 35 ms: 31 ms hashing, with per-file Python overhead (about 8 µs a file
holding the GIL, 8 ms in all) overlapping it. That is within 15% of the bound. For trees, Python's
`os.walk` and `lstat` cost about 20 µs a file, 0.6 s for 30,000 files, 100× the bound.

**Trees now (sol-all, 2026-10-30; by count, not measured).** `engine.tree_digest` runs
`blaze-v2-proof '{"tree": ..., "skip": [...]}'` (`crates/blaze-v2-proof/src/tree.rs`), found
beside the blaze under test (`engine.proof_beside`). It lists each directory as a rayon task,
takes each entry's type from the listing (`d_type`, no `stat`), and hashes files in tasks of up
to 64: an open, an `fstat` for the mode and size, one read into a buffer the thread reuses (mapped
from 1 MiB). Per file that is about 3 syscalls, about 2 µs, ÷ 16 cores ≈ 4 ms for 30,000 files,
plus 300 MB of BLAKE3 across the cores (3 ms) and the process start (about 2 ms): **about 9 ms,
1.5× the 6 ms bound**, against 100× before. What is left is the process start and the `fstat`
(`statx` batched by io_uring would fold it into the open). The bytes hashed are the old walk's, so
kept references still match; `proof.py selftest PATH` checks each digest against the old walk
(`old_tree_digest`) after every kind of change. Where the old walk skipped an unreadable directory
in silence, this one stops naming it.

**State of the art.** `b3sum` maps each file and hashes it across cores with BLAKE3's tree mode.
That is the same work done here, at about 6 GB/s a core. `sha256sum` is one core at 2 GB/s.

### Trace join (`crates/blaze-v2-proof/src/join.rs`, `wire.rs`)

**Workload.** The oracle's `under` (what an incremental build kept that a build from scratch
says is stale) and `over` (what it reran for nothing). These are taken over traces of nushell's
size: n = 5 M keys and m = 50 M edges. A trace is about 1.27 GB: keys 34 B, a fingerprint 16 B,
an edge 4 B for its read plus 16 B for what it saw, and the two bitsets.

**Bound.**
- Reading both traces: 2 × 1.27 GB ÷ 400 GB/s = 6.4 ms. Only the columns the join touches are
  read; with the edges read once, that comes to about 3.75 ms.
- Probes: each of the 50 M edges of a kept key checks the fingerprint of the key it reads, a
  random access into an 80 MB array (a DRAM miss). With 10 to 20 misses in flight a core, a probe
  costs about 8 ns on one core: 50 M × 8 ns ÷ 16 = 25 ms.
- Checking the trace's invariants in place: about 1 ms.
- Total: about 30 ms on 16 cores, about 450 ms on one.

**Was.** Each trace was decoded from the derived serde CBOR, which writes a `[u8; 32]` as 32
items: 35 items a key and 17 an edge, roughly 1.2 G items a trace. That is about 1.5 s a trace,
three traces a case. Then `under` ran on one core (about 450 ms). In all about 5 s, 150× the bound.

**Changed.**
- `wire.rs` is version 2 of the trace file: a CBOR map of the version plus one byte string per
  column, fixed width and little-endian.
- `View::parse` borrows the columns in place (`as_chunks`) after a parallel pass that checks
  them. The pass checks that keys are strictly ascending, that `edges_from` runs from 0 to m
  without going down, and that every read is below n. A bad file is refused with the place named,
  never patched up.
- `join.rs` takes both traces through the `Columns` trait, implemented by `Trace` and by `View`.
  It splits the keys into ranges, one or more a core and at least 16,384 keys each. Each range
  seeks its start in the other trace (a gallop, then a binary search).
- The pieces' results are concatenated in key order and sorted exactly as the serial `crate::under`
  sorts. Tests check that `under` and `over` equal the serial ones for 1 to 7 pieces, on `Trace`
  and `View`, with and without roots.
- `main.rs` maps the traces lazily and joins the views. It builds owned traces only for a nameres
  case.
- Two lines of `lib.rs` (`pub mod join; pub mod wire;`, and `Trace` dropping its derived serde
  for `wire.rs`'s) are a request in `v2/requests/proof.md`, because `lib.rs` is shared. Until that
  lands, `main.rs` does not build.

**Now.** About 35 ms on 16 cores once the request lands: 25 ms of probes, 4 to 6 ms of streaming,
1 ms of checking, and thread start-up. That is within 15% of the bound.

**State of the art.** rustc reads its dep graph by decoding every node and edge (LEB128) at
start-up, at a few hundred MB/s. salsa keeps none on disk. Columnar formats read in place
(Arrow IPC, FlatBuffers) cost no decode, which is what `wire.rs` does. Splitting the join into key
ranges, one per core, is morsel-driven parallelism (HyPer, DuckDB) applied to a merge join.

```bound
{"name": "proof: trace join, 5 M keys and 50 M edges", "bound": 30.0, "unit": "ms", "program": "blaze-v2-proof", "args": ["{\"bench\": \"join\"}"]}
```

Both blocks run `blaze-v2-proof`'s bench mode (`src/bench.rs`). The bench makes the workload in
memory, times only the work the bound counts, and prints `{"value": MS}`. The join bench joins a
trace with itself: every key is matched and every kept edge is probed, which is the worst case.
The bounds are for the 16-core machine; a VM with fewer cores reports against the same figure, so
run them on one of that size.

### Corpus run (`harness/v2/corpus.py`)

**Workload.** Every project built and tested twice, by blaze and by rustc. The projects are 52
crates.io crates plus pinned blaze, nushell, ripgrep and helix.

**Bound.** The critical path: max(Σ work ÷ 16 cores, the longest single project's own critical
path). The rustc side of a project costs nothing on a rerun, because its kept result
(`engine.Kept`, keyed by toolchain, source and steps) is reused, so after the first run the bound
is blaze's side. The harness's own work (unpacking, hashing, comparing, writing the report) must be
under 1% of that.

**Was.**
- Two projects ran at once (`jobs`, default 2), each with the blaze side and then the cargo side,
  one after the other.
- Projects started in input order, so a big workspace listed late (blaze, which builds for
  minutes) could run alone at the end while the other slot sat empty.
- The harness's overhead was not measured.

**Changed.**
- Projects start largest first (`largest_first`): the four workspaces before the crates.io
  crates, each group in input order. This is Graham's LPT rule, whose makespan is at most 4/3 of
  the optimum and, here, removes the lone tail. The report keeps the input's order.
- The report now carries `harness_cpu_s`, `children_cpu_s`, `harness_share` and `wall_s`, read from
  `os.times()` around the pool. The share is the harness's own CPU over its children's, the
  quantity the 1% bar is about.
- Hashing in the comparisons is the BLAKE3 path above.

**Now.** The estimated overhead is 0.2 to 1%, now measured on every run by `harness_share`. The
gap left is idle cores. With two projects at once, a small crate's build of a few seconds does not
fill 16 cores, and the blaze and cargo sides of one step do not overlap. The fix is to choose
`jobs` from the measured busy share of the cores (as `budgets.py` measures for `cold-blaze`), or
to run a step's two sides at once. Either is a change to what a run measures, so it waits for a
measured `harness_share` and busy share to say which.

**State of the art.** crater builds each crate in its own container, on both toolchains on every
run, and keeps no reference result. Here the reference side is kept, so a rerun costs one side.

### Budgets: how many runs each needs (`harness/v2/budgets.py`)

**Workload.** Each timed budget's samples, judged by the median against the limit.

**Bound.** The runs needed for a 95% interval of the median narrower than 10% of the bound. For
timings with coefficient of variation cv, the median's standard error is √(π/2) · cv/√n. An interval
2 × 1.96 × that wide is under 10% when n ≥ (2 × 1.96 × √(π/2) × cv ÷ 0.1)² ≈ (49 cv)². The
order-statistic interval below needs at least 6 runs.

| budget | cv (expected) | runs |
|---|---|---|
| `noop` (ripgrep, a few ms) | 5 to 10% | 7 to 25 |
| `body-edit`, `big-crate` | 3 to 5% | 6 to 7 |
| `warm-start` | about 5% | 7 |
| `restart` | about 10% | 25 |
| `ci`, `cold-blaze` (minutes) | about 2% | 6 |
| `dev-speed`, `release-speed` (each side; the ratio's error is both) | 1 to 2% | 6 a side |
| `bounds` (the benches above, and other lanes' programs) | 1 to 3% | 6 |
| memory, layout and count budgets | none (exact) | 1 |

**Was.** A fixed count per budget (`runs`, default 10), judged by the median with no interval.
Ten runs were too few for `restart` and `noop` at their noise, and more than needed for most of
the rest.

**Changed.** Every timed budget samples until its interval settles. The interval is a
distribution-free 95% interval of the median from binomial order statistics, so it assumes
nothing about the timing's tails. A budget stops once that interval is at most 10% of the limit
wide, or lies wholly on one side of the limit (`enough`), and never above `most_runs` (default
60). `runs` stays the least. `dev-speed` and `release-speed` run until both sides' medians are
known to 5%. Each outcome reports `runs` and `runs_needed`, which is `runs_for(cv)` from its own
samples, so the table above gets replaced by measured figures run by run. Checking the interval
after every run lowers its real coverage somewhat below 95%. The 60-run cap and the 10% width,
far tighter than the 10% slack the budgets allow, keep that from deciding a verdict.

**State of the art.** hyperfine runs a fixed count (at least 10, or 3 s), reports the mean and
standard deviation, and flags outliers. criterion.rs takes 100 samples and a bootstrap interval.
Neither stops when the answer is known, nor sizes the run to the question.

### dbsp's case check (`crates/blaze-v2-proof/src/dbsp.rs`) (speed-sol2-proof)

**Workload.** `blaze-v2-proof '{"dbsp": KIND, …, "before": …, "after": …}'`, once for each edit
of `harness/v2/dbsp/edits.py`: the traces of the builds before and after one edit, held to the
edit kind's case. The worst case is nushell's size, n = 5 M keys and m = 50 M edges a trace. The
build after an edit runs r keys, about 10³ for an impl edit, of which w are of a watched kind
(the resolver's, the slices', and those the case counts).

**Work, counted.**
- Parsing and checking each trace in place (`View::parse`): the keys (34 B) and the reads
  (4 B), checked across cores. That is 2 × (170 MB + 200 MB) ÷ 400 GB/s ≈ 1.9 ms.
- Each of the case's 3 or 4 bounds and the 13 round bounds: two binary searches for the kind's
  run in each trace (about 4 × 23 key compares), a scan of the run's `has` bits, and a merge of
  the two runs. That is about 17 × 2 µs plus the runs' lengths, under 1 ms in all.
- The sliced kinds' edges in `after`: one key load for each edge read, at most a few thousand.
- The reruns: the `ran` and `has` bitsets of `after`, n ÷ 8 bytes each (1.25 MB, about 50 µs at
  one core's 25 GB/s), and for each of the w watched reruns, a seek in `before` and one binary
  search in `after` (about 23 probes, the first ~10 DRAM misses, about 1 µs) for each edge it
  read before.

**Bound.** About 2 ms of checking plus 3 ms for the reruns and their edges: **about 5 ms**, set
by the trace check. An edit with no rerun costs that check and nothing else.

**Was.** `main.rs` copied both views out (`View::to_trace`): 2 × 1.27 GB written and read, plus
the page faults of 2.5 GB of fresh allocations. That is about 2 × (1.27 GB × 2 ÷ 10 GB/s) + 0.6 M
faults × 0.25 µs ≈ 650 ms on one thread. Then the last pass tested `ran` and `has` bit by bit for
all n keys (5 M iterations, about 5 ms). All told about 660 ms, **130× the bound**.

**Changed.**
- `dbsp::check` is generic over `join::Columns`, as `join::under` and `join::over` are. `main.rs`
  passes the two `View`s of the mapped files, and no column is copied.
- `Columns` gained `has_word` and `ran_word`. The reruns are found by `join::ran_results`, one
  word of each bitset per 64 keys and a `trailing_zeros` per member, in the same ascending order.
  It masks off bits a file sets past its last key, which `View::check` does not look at.
- The kind runs (`join::of_kind`), seeks, results and edge comparisons are `join`'s, shared with
  the cross-core join.
- Tests: `a_check_in_place_answers_as_on_owned_traces` holds the check on views, and on one view
  with one owned trace, to the check on owned traces, for every case and three `Touched`s over 24
  seeded pairs. It also holds `ran_results` to a bit-by-bit walk.
  `a_check_of_a_build_that_reran_nothing_allocates_nothing` holds the check of a settled build to
  zero allocations; `the_view_answers_what_the_trace_does` checks the new words.

**Now.** About 2 ms of checking plus the reruns: **within 10% of the bound**. The check of the
traces is the same parallel pass `join` uses. What `proof` should measure: the `dbsp` command's
wall time on a pair of nushell-sized traces (the join bench's trace, edited), against 5 ms.

### Name resolution's case check (`crates/blaze-v2-proof/src/nameres.rs`) (speed-sol2-proof)

**Workload.** `blaze-v2-proof '{"incremental", "scratch", "before", "slot"}'` for a fixture slot
with a `nameres` case: three traces held to the slot's case.

**Bound.** The same shape as dbsp's: the parallel check of three traces (about 3 ms at nushell's
size), the kinds' runs, and the reruns' edges. About **5 ms**.

**Now.** `main.rs`'s `compare` copies all three views (`to_trace`) before `nameres::check`. That
is about 3.8 GB copied, roughly 1 s, **200× the bound**. It is not closed here: `nameres.rs` is
held by lane frontend (window `frontend-next1`, extending its cases), so the exact edit (the
same as dbsp's, with `join`'s helpers already `pub(crate)`) is queue item 00897.

### The harness's own share of a timed command (`harness/v2/engine.py`, `budgets.py`) (speed-sol2-proof)

**Workload.** Every timed budget times a `blaze` client command run by Python. The smallest
limits are `noop` (5 ms, ripgrep), `body` (30 ms) and the restart budget's warm check (about
12 ms).

**Work, counted.** What the harness must do for a timed run: spawn the client (`vfork` and
`exec`, inside the time because a user pays it too) and reap it. What it need not do inside the
time: decode the output, translate newlines, parse `blaze-work` JSON lines, check who served
the build.

**Bound.** Python's `subprocess.run` with two pipes: building the environment array, the
`vfork`/`exec` and the `poll` loop over the pipes, about 0.1 to 0.3 ms over the exec of the child
itself. That is 2 to 6% of the 5 ms `noop` limit.

**Was.**
- `engine.run` decoded the output (text mode) before stopping its clock.
- `Lane.build` timed around `engine.V2.build`, which also parses the `blaze-work` lines of
  JSON output.
- `restart`'s warm check timed `served(...)` too.
- For a few kilobytes of output, the decode and parse cost about 0.05 to 0.5 ms. With the spawn,
  the harness's share of a `noop` sample was up to about 0.8 ms, **16% of the limit**.

**Changed.**
- `engine.run` stops its clock (`perf_counter`) when the child is reaped, then decodes with
  `engine.text`. That function is exactly what text mode did (UTF-8 with replacement, then
  `\r\n` and `\r` to `\n`), and `budgets.py selftest` pins it.
- `Lane.build` and the restart check take `Ran.seconds`, the child's time, rather than timing
  their callers' work.
- `noop` reports `harness_floor_ms`: the median `engine.run` time of `true`, nine runs. That is
  the most of a sample that is the harness's own (it includes `true`'s exec).

**Now.** The decode and parse are out of every timed sample, leaving the spawn's 0.1 to 0.3 ms:
**within 10% of the bound for every budget at or above 3 ms**. What `proof` should measure:
`harness_floor_ms` against 10% of the `noop` limit. If it is above 0.5 ms, the next step is
spawning the client with `os.posix_spawn` and pipes the harness reads itself. That would need a
working-directory action Python does not expose, or `blaze -C`.

Whole builds are timed the same way elsewhere (`reachable.py`, `pure_scripts.py`, `determinism.py`,
`share.py`, `oracle.py`, `corpus.py`). There the decode and parse are under 1% of a build of
seconds, inside the bar, and `reachable.py` and `pure_scripts.py` are lanes cold-v2's and
pure-scripts' now.

### The rest of `harness/v2` (speed-sol2-proof)

Each other script orchestrates builds, and its own work is that bound's 1% share: `corpus.py`
measures that share as `harness_share`. The work it adds per step, counted from the code:
- `oracle.py`: one `blaze-v2-proof` join per step (the trace join above);
- `fixture.py` and `dbsp/edits.py`: writing a few files per edit, microseconds;
- `share.py`: one build per program, `jobs` at once;
- `tightmemory.py`: a cgroup settle poll (100 ms) between runs, outside any timing;
- `macroeval_corpus.py`, `report.py`, `bounds.py`, `proof.py`: setup and output, once a run.

None does per-byte or per-file work in Python on a timed path. The SHA-256 left (`corpus.py`'s
crates.io check, `Kept`'s keys, `pure_scripts.py`'s lock digest) hashes kilobytes. `columns.rs` is
test-only. `harness/v2/frontend/` and `rejections.py` were held by `frontend-fast-diff-next1`
(live) and were not audited here.

### Bounds as budgets

Every lane's `speed-of-light.md` says what proof measures. For each component a program can
measure, the lane also writes, beside that component's arithmetic, a fenced block with the info
string `bound` holding one JSON object, as the two blocks above do. Its fields:

- `name`: unique across lanes.
- `bound`: a number, in `unit`.
- `unit`: what the program's value is in.
- `program`: a program the build delivers beside `blaze`.
- `args`: the program's arguments.
- `rate`: true when more is better, such as a throughput.

`harness/v2/bounds.py` reads every such block in `docs/v2/plan/lanes/*/speed-of-light.md` into a
typed `Bound`. It refuses a block that is not exactly one such object, a bound not above 0, and
two bounds of one name, each refusal naming the file and line. `budgets.py`'s budget `bounds`
makes one outcome a bound, `bound-<name>`. Its limit is bound × 1.1, or bound ÷ 1.1 for a rate.
The outcome runs the program until its interval settles, as above, and takes `value` from the
JSON object on the program's last line of output.

So a lane adds a budget by writing one block and shipping one program that does the reference
workload once. No harness change is needed. A component with no such program yet stays prose:
the outcome is missing, so proof has nothing to measure for it, and nothing for it is reported as
passing.
