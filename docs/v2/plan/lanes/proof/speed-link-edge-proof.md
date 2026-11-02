---
status: plan
summary: blaze v2 proof lane, speed pass: the oracle's byte compares, the corpus and rejection references kept by content, the budgets' statistics; before and after.
date: 2026-10-29
---
# speed-link-edge-proof: the proof lane's hot paths

Part of [proof](../proof.md). Paths: `crates/blaze-v2-proof/`, `harness/v2/`. Nothing here was
built or run (docs/v2/plan/goals/principles.md); the numbers are counts read off the code, and
throughputs are the published ones of the technique, to be measured when the coordinator builds.

## Where a proof run spends its time

1. Comparing what the two sides built: every output file of `target/debug` hashed
   (`engine.outputs`), and on a mismatch the first differing byte located
   (`engine.first_difference`, `blaze_v2_proof::bytes`).
2. Building the references: plain cargo's `test` per corpus crate (`corpus.py`) and rustc per
   `tests/ui` unit (`rejections.py`). These dominate a rerun by far: minutes per crate.
3. Reading the database's traces (hundreds of MB for nushell) and joining two of them
   (`blaze_v2_proof::{under, over, apart}`).
4. The budgets' timings (`budgets.py`): the statistics decide whether a run settles a budget.

## State of the art, and how this compares

| Component | Benchmark | Before | After |
|---|---|---|---|
| First differing byte, Rust | glibc `memcmp` AVX2/EVEX, `cmp(1)`: memory-bound, ~10-20 GB/s a core | none in the crate | mapped, 64/32/16 bytes a vector (AVX-512BW, AVX2, SSE2, NEON) chosen at run time, 4 vectors and one test per loop, overlapping last vector: memory-bound, the offset itself returned (memcmp gives only a sign) |
| First differing byte, Python | the same, through `bytes.__eq__` (memcmp) | 64 KiB `read` (2 new `bytes` each), then a Python loop over up to 65,536 bytes | two 1 MiB buffers made once, `readinto`, whole-buffer memcmp, the differing MiB halved by memcmp to 64 bytes: at most 64 bytes compared in Python |
| Output digests | BLAKE3 SIMD ~3-6 GB/s a core; SHA-256 with SHA-NI/ARMv8 SHA2 ~1.5-2.5 GB/s a core | `read` in 1 MiB chunks (one allocation per MiB), one file after another, an `lstat` per entry | `scandir` (the type from the listing, `stat` only for the chosen), mapped and hashed in one call with the GIL released, every core at once, largest first |
| Trace load | zero-copy mapped read | `fs::read` into a `Vec` (one copy of the whole trace) | `bytes::Mapped` with `populate` (no copy, one fault-in) |
| Trace build | one hash probe per key (rustc's dep graph, salsa) | `BTreeMap` insert per result, a binary search per edge in `finish` | `Vec<Row>` pushes; `finish` names each key through one table indexed by the key's own content hash (one probe per edge), sorts only the distinct keys |
| Trace joins (`under`, `over`, `apart`) | merge join of sorted columns | binary search per key; `apart` a lookup per key and a sort of the result | merge join with a moving cursor (`seek`), amortized one comparison per key; `apart` one merge, no sort |
| v1 log reads (`work_since`) | read each byte once | every 50 ms poll reread and redecoded every log's whole tail since the mark | incremental: each poll reads only the bytes appended since the last |
| Reference results, corpus | crater: every crate built and tested by both toolchains on every run | cargo's `test` run for every crate on every run | kept by a content key (`engine.Kept`): toolchain identity, platform, the checkout's tree digest after `cargo fetch`, arguments, cargo config files outside the checkout, the environment with the run's paths written as placeholders. An unchanged crate runs 0 cargo processes on a rerun |
| Reference results, rejections | compiletest: rustc per unit per run | rustc per unit per run | kept by the content of what rustc read (its dep-info: every file and its hash, every env var it read), so a rerun runs rustc only on units whose inputs changed |
| Report order | one input, one report | `as_completed` order (thread timing) | submission order in `corpus.py` and `rejections.py` |
| Budget statistics | hyperfine: warmup, mean, stdev, min/max, outlier warning, at least 10 runs | median of 5, samples listed, no warmup for the no-op | 10 runs by default, one warmup no-op build, mean/stdev/min/max, outliers by modified Z-score (Iglewicz-Hoaglin, hyperfine's rule), and the distribution-free 95% interval of the median (order statistics of Binomial(n, 1/2); 6 runs at least). A budget whose limit falls inside the interval is reported `settled: false` with "more runs would settle it" |

## Per comparison, before and after

- **Bytes read and hashed**: each output is read once on both sides as before (hash first: the
  32-byte digests compared, bytes compared only on a mismatch). What changed is how: before,
  one 1 MiB `bytes` allocation per MiB per file and one core; now no copy and every core. A
  mismatch was located by up to 2 x 64 KiB reads per step plus a Python byte loop; now 1 MiB
  reads into reused buffers and memcmp to the last 64 bytes.
- **Allocations per artifact**: before, `size / 1 MiB` buffers to hash plus a `Path` per entry
  and an `lstat`; now O(1) (one map) and a `stat` only for chosen files. In Rust, a trace went
  from one copy of the file plus a `BTreeMap` node per ~11 results to no copy and one `Row`
  push per result into a vector grown by doubling.
- **Processes per crate**: before, cargo `test` (corpus) and one rustc per unit (rejections) on
  every run; now 0 for an unchanged input on a rerun. `rustc -vV` and `cargo -vV` run once per
  harness run, not per crate.
- **Throughput**: the Rust compare and the mapped hashing are memory-bandwidth bound; the
  Python compare is memcmp-bound except for the final 64 bytes.

## Where this does not match the benchmark yet, and why

- **SHA-256, not BLAKE3, in the Python harness.** `blake3` is not a dependency of the repo's
  Python environment (`pyproject.toml`, `uv.lock` are not this lane's paths). With hardware
  SHA-2 and every core it is ~2-3x below BLAKE3 a core. Switching changes the key domain
  (`"blaze v2 proof <kind> 1"` would go to 2) and nothing else. Requested in
  `v2/requests/speed.md` under this agent's heading.
- **The Rust compare does not hash first.** When both files are on disk, one SIMD pass that
  stops at the first difference reads each byte at most once; hashing both reads every byte and
  then needs a second pass to locate. Hash-first pays only against a stored digest, which is how
  the Python harness uses it (`outputs`, `Kept`).
- **The Python bisection** slices `bytearray`s (`x[lo:mid]`), copying at most 2 MiB once per
  mismatch. It runs only on a mismatch, so it stays.
- Corpus parallelism is a thread pool over projects (2 at once by default, each with a daemon of
  its own), not work stealing: a project's builds already use every core, so more parallelism
  between projects would only contend.
