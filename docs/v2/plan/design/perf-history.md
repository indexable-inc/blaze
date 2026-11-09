---
status: plan
summary: Performance history of the programs blaze builds (lane perf-history). Each profiled run is one row set in the profile history (the run with its exact tree, blaze, binary and host; per function self and total samples, by path and by code identity), and `blaze perf diff <run> [<base>]` gives per function the change in time with a noise bound and the cause class read from the identities (its code changed, built with other settings, a callee changed, or same code and callees). Built: the code map each link writes, the rows and their Parquet round trip, the identity join, the diff (text and JSON) and `blaze perf record|runs|diff`. Designed: the `--prof` switch that records on its own, call and instruction counts.
date: 2026-11-02
---
# Performance history: compare runs and see what changed

Andrew, 2026-11-02: "blaze should also include enough data (similar to PGO) that you can
compare performance across runs easily and see what improved."

A profile tells you where one run spent its time. To see what improved, you need two runs
whose functions line up, a difference you can trust, and the reason behind it. blaze knows
the reason better than any outside profiler, because it built both binaries and knows each
function's code identity. This page describes how blaze keeps that data and how
`blaze perf diff` uses it.

## What a run records

A *run* is one or more repetitions of one program blaze built. Each repetition is one
`.ixprof` that ix-prof wrote (docs/ix-prof-format.md). `blaze perf record` reads the
repetitions and writes one row set into the profile history (`history::dir_in(state)`, the
directory `blaze debug sql` reads). Each run gets two files there:
`perf-<run>.perf_runs.parquet` and `perf-<run>.perf_functions.parquet`. Their tables are
declared in `crates/blaze-profile/src/schema.rs` beside the others, so
`blaze debug sql "SELECT * FROM perf_functions"` works too. The history's cap never deletes
them: their names are not `<build>-<seq>`.

`perf_runs` has one row per run:

| column | meaning |
|---|---|
| `run` | 16 hex digits: a hash of the repetitions' file contents. Recording the same files again is refused, never a second run |
| `format` | the perf rows' version (1). A reader refuses any other version by name (rule 23) |
| `target` | the program, as the profile names it (its executable's file name) |
| `started_unix_ns` | the first repetition's start |
| `reps` | how many repetitions |
| `tree` | the `blaze-v2-proof tree` digest of the workspace (rule 19); null with `gaps` saying why |
| `blaze` | BLAKE3 of the blaze that recorded it |
| `binary`, `build_id` | BLAKE3 of the executable that ran (the sampled module whose file name is the program's, hashed at record time; null, with a gap, when it was written after the run began), and its build id as the profile's frames name it |
| `host`, `arch`, `os` | where it ran (`host` is the recording machine's `hostname`) |
| `cpu_period_ns`, `sampler` | the weight of one sample, and how the sampler ran |
| `samples`, `unmapped` | on-CPU samples, and those in code no code map names |
| `wall_ns` | the repetitions' wall time, summed |
| `maps` | the directory holding the code maps the rows were joined with |
| `gaps` | each input the run lacks, in words (no tree, no map for build X, no call counts) |

`perf_functions` has one row per function per repetition:

| column | meaning |
|---|---|
| `run`, `rep` | the run and the repetition (0-based) |
| `function` | the instance's demangled path, from the code map; the frame's symbol when unmapped |
| `module` | the image it is in |
| `identity` | what the function is across builds (`Described::identity`: item, which instance, type arguments). An edit keeps it. This is the join key |
| `code` | its own code: H(code hash, the facts its type arguments' code reads) |
| `key` | its instance key: `code` plus the codegen settings (toolchain, backend, opt level, target) |
| `deep` | a Merkle hash of `key` and of the `deep` of everything it refers to, with recursion hashed as one strongly connected component |
| `self_samples`, `total_samples` | samples with it as the running function, and samples with it anywhere on the stack (counted once per sample) |
| `calls`, `instructions` | deterministic counts, null until recorded (see "Counts") |
| `gap` | for a row with no identity: why (the frame's build has no code map, or the code is not blaze's) |

Functions are named by path *and* identity, never by address. The address only resolves a
sample, through the code map of the build that ran.

## The code map

A profile names each frame by build id and image-relative address
(`ix_prof::profile::Frame`). While `<target-dir>/perf` exists, each link writes
`<target-dir>/perf/maps/<image>-<hash>/<build id>`, in the format `blzpmap1`
(`crates/blaze-profile/src/perf/map.rs`). Per node it holds the address, size, identity,
`key`, `code`, demangled name and edges. A node is each function and static of the build's
reach graph (`blaze_codegen::driver::Built::reach`; its edges include the callees a body
inlines or builds in), plus each function of the image the graph has no node of (a folded group's body, by its key).
A static is named `<static INSTANCE>`, since the build keeps no path for it. The `layout` maps
(docs/v2/plan/design/code-layout.md) hold only identity and address. This map adds what the
cause classes need. The newest 8 maps of an image are kept. A run already recorded keeps
`deep` in its rows, so a collected map only costs the names of the callees that changed, and
the diff says so.

`--prof` (designed by the pprof lane, `/tmp/blaze-lanes/v2-pprof-format.md` section 0; the
flag's name is Andrew's decision) will create `<target-dir>/perf`, build, run the program with
`IX_PROF=<target-dir>/perf/incoming/<n>`, then call the same record step on what lands
there. Until it lands, `blaze perf record` does that step by hand, for any program that starts
ix-prof (`ix_prof::start_from_env`; blaze itself does).

## Cause classes

`blaze perf diff` joins the two runs by `identity`. When a run has several instances of one
identity (two settings), they merge into one row whose `key`, `code` and `deep` hash the sorted
set. For each function present in both runs:

1. **its code changed**: `code` differs. Its source or its type arguments' facts changed.
2. **built with other settings**: same `code`, different `key`. The toolchain, backend, opt
   level or target changed. While `Toolchain::id` is still the blaze executable's hash
   (`temporary_executable_id`, code-identity.md port 4), every blaze rebuild lands here.
3. **same code, a callee changed**: same `key`, different `deep`. When both code maps are
   still kept, the diff names the callees whose own `key` changed (at most 5, found by walking
   down the nodes whose `deep` differs). It does so only for rows beyond noise.
4. **same code and callees: layout, inlining or environment**: `deep` equal. Nothing blaze
   built for it changed. The difference comes from where code sits, the whole-program inputs of
   release inlining, or the machine.

A function in only one run is **added** or **removed**. A row with no identity on either side
is **unknown**, and the diff says which input is missing (rule 3). It is never guessed from
the name. Unmapped rows still line up, by module file name and symbol, so their time shows,
but their class is always unknown.

## The noise bound

The diff compares each function's mean time per repetition, self and total, in nanoseconds
(samples × `cpu_period_ns`). For each side it takes the variance across that side's
repetitions, with a Poisson floor: the variance is never less than the mean count, because a
sampled count has at least sampling noise. The bound is
`Z × sqrt(var_a / n_a + var_b / n_b) × period`, with `Z = 3`.

- A change inside the bound is reported as "within noise". It is never listed as faster or
  slower, and the summary counts these changes and sums their net effect. This is the rule
  that noise is never presented as a win.
- With one repetition on either side, the bound covers sampling noise only, and the header
  says so: "repeat the run to bound run-to-run noise".
- Different hosts, sample periods or architectures are named in the header as reasons the
  runs may not compare.

Rows are sorted deterministically (rule 22): changes beyond noise first, then by the size of
the self change (descending), then by function path, then identity. The text lists the rows
beyond noise (`--all` lists every row); `--json` prints every row (`perf::diff::Diff`, serde)
with every number, so a script or agent never parses the text. RUN defaults to the latest run
recorded, BASE to the run of the same program before it, and either may be a prefix of one
run's name.

## Counts (designed)

Sampled time is noisy by nature. Deterministic counts don't vary between runs:

- `calls`: PGO instrumentation (`Pgo::Generate`, docs/v2/plan/design/codegen.md) counts
  each function's entry. The link side of `Generate` is still missing. When it lands, record
  reads the `.profraw` and joins each record by its PGO name (`blaze.pgo.<item and type
  arguments>`). The code map then gains each node's PGO name. A changed count is exact, so the
  diff reports it with no noise bound.
- `instructions`: retired instructions per function from a counting sampler
  (`perf_event` `instructions:u` on Linux), CodSpeed's and rustc-perf's quieter metric.
  ix-prof's sampler takes time samples only today.

Until then both columns are null, and `perf_runs.gaps` says "calls: not recorded (no
instrumented build)". The diff states that, and does not leave it out silently.

## Prior art

Searched on 2026-11-02: GitHub (`codspeed`, `gungraun`/`iai-callgrind`, `bencher`,
`differential flame graph`, `pprof diff`, `perf regression detection`) and the projects' docs.

- **rustc-perf** (perf.rust-lang.org): a result counts as significant only past an IQR fence
  over that benchmark's history, and its compare page sorts by "significance factor" (how far
  past the threshold), not by raw change. Taken: sort by significance, and show the bound beside
  each change. Not taken: history-based thresholds per benchmark. Per function, the
  repetitions of the run itself are the better sample, with the Poisson floor when they are
  few.
- **Bencher**: threshold models (percentage, z-score, t-test, IQR) per branch, testbed and
  measure. Taken: compare only like with like (the header names host and period differences),
  and use a variance-based bound for small samples. Not taken: a server and alerts.
- **CodSpeed**: runs each benchmark once under a simulated CPU (instruction and cache counts,
  syscalls excluded) and shows base, head and diff flame graphs coloured slower, faster, added
  and removed. Taken: the added and removed classes, and counts as the low-noise metric to
  come. Not taken: Valgrind simulation. blaze builds the code itself, so its counts come from
  instrumentation it compiles in.
- **gungraun** (formerly iai-callgrind): one-shot Valgrind counts per benchmark. Taken: the
  case for deterministic counts. Not taken: running under Valgrind.
- **pprof `-diff_base`, `blaze pprof diff --base`, Firefox profiler compare, speedscope
  differential flame graphs (stackweigh)**: these subtract two profiles by function name. None
  knows whether a function's code changed, so a renamed or regenerated function shows as noise,
  and a change from a callee or from layout can't be told from one in the function itself. That
  is what code identity adds.

## What is built, what is not

Built (type-checked; tests written, run by the coordinator):

- `crates/blaze-profile/src/perf/`: `map.rs` (the `blzpmap1` format, writer and reader, the
  Merkle `deep` over strongly connected components, bounded), `record.rs` (profiles plus maps
  into rows, with named gaps), `rows.rs` (the Parquet writer and reader, `format` checked),
  `diff.rs` (join, noise bound, cause classes, text and JSON), `tests.rs`.
- `schema.rs`: `perf_runs` and `perf_functions`. `sql/views.sql` has their views.
- The link writes the code map while `<target-dir>/perf` exists
  (`crates/blaze-cargo/src/serve/unit/perf.rs`, called from `codegen.rs` `link_at` beside the
  layout map). `Reach::graph` in blaze-codegen exposes the reach graph's arrays.
- `blaze perf record [--target-dir DIR] [--workspace DIR] FILE.ixprof...`, `blaze perf runs
  [--json]` and `blaze perf diff [RUN [BASE]] [--json] [--all]`
  (`crates/blaze-cargo/src/cli/perf.rs`). The workspace defaults to the nearest directory with a
  `Cargo.lock`, and the target directory to `CARGO_TARGET_DIR` or `<workspace>/target`.
- `docs/profiling.md` lists both tables' columns.

Designed only: `--prof` on run, test and bench, and the ix-prof link-in it needs (pprof lane);
`calls` from PGO; `instructions`; grouping a `blaze test --prof` run by its `test` spans; a
differential flame view in the explorer, read from the same rows.
