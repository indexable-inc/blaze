---
status: plan
summary: blaze v2 lane fastcompile, ideas 11 and 12 (scheduling from history; blaze built with PGO and BOLT): crates/blaze-history, its fixed-size per-key records in a seqlock table that readers never lock, persisted in the daemon image; the `Predict` hook the plane asks; tools/pgo, which trains, builds, BOLTs, gates and publishes blaze through blaze; and the state of the art each matches or beats.
date: 2026-10-29
---
# fastcompile: scheduling from history and blaze built with PGO and BOLT

Ideas 11 and 12 of [compile-time.md](../../design/compile-time.md). Owned here:
`crates/blaze-history/`, `tools/pgo/` and this file. The hooks in other lanes are requests in
`v2/requests/fastcompile.md` (`fastcompile-sched 1` to `5`).

## 11. Scheduling from history (`crates/blaze-history`)

The plane orders ready nodes by priority, a node's cost plus the largest priority among its
dependents (the critical path), and admits a node only when its memory fits the budget. Today
every cost is 1 and memory is a per-kind running estimate (`blaze_graph::admission::Estimates`).
With history, both come from what the same task did before.

### What is kept

- **Per key**: a `Record` of one (task kind, `QueryKey::task_key`) pair, 64 bytes: the kind, the
  first 128 bits of the key's BLAKE3 hash, how often it was seen, when it was last seen (a clock
  the writer advances per observation), its peak bytes and its last four run times in
  microseconds. The predicted duration is the median of the runs kept (one outlier, a cold
  page cache or a descheduled thread, does not move it); the predicted peak rises at once to a
  higher observation and halves towards a lower one, so admission errs towards fitting.
- **Per kind**: `Times`, an integer exponential mean (weight 1/8) of every run of the kind, and
  the existing `Estimates` for its peak. A key never seen is predicted from its kind; a kind
  never seen from the configured unseen values (1 ms, 64 MiB).

The kind estimate for an unseen key is **not a fallback** in the sense of AGENTS.md rule 3: a
prediction only orders and admits work, it never changes what is built or what it outputs, and
a key that has no history has no better input. A wrong guess costs time, never correctness, and
the first run of the key replaces it.

### The table

`table::Writer` owns a power-of-two array of 64-byte slots in buckets of four (a set-associative
cache, like a CPU's): a key goes to bucket `(key[0] ^ kind * φ) & mask` and to the way that
holds it, else a free way, else the way seen longest ago (LRU within the bucket, by the writer's
clock). The default 65,536 slots are 4 MiB; memory never grows, keys are evicted by age, kinds
are kept (their table has one entry per kind, a few hundred).

Readers (`table::Reader`, cloneable, `Send + Sync`) never lock: each slot is a seqlock over
eight `AtomicU64`s (the sequence and seven packed words), written by the one writer (the
`Writer` is `Send` and not `Sync`, so the type system keeps it single) and read with an
acquire load of the sequence, relaxed loads of the words, an acquire fence and a recheck,
retrying while a write is in flight. All of it is safe Rust: the crate is
`#![forbid(unsafe_code)]`. A lookup loads its bucket's four lines at once; the writer (the
plane's own predictions and observations) compares every way with no branch on what it holds,
and a reader stops at the first empty way, since a bucket fills from its first way and never
empties one.

### Persistence

`History::encode` writes, in a fixed order, `blzhis01`, the slot count, clock and eviction
count, the encoded `Estimates`, the kinds sorted, and the occupied slots in slot order: the same
history makes the same bytes. `decode` checks every field (magic, lengths, sort orders, that
each record sits in its bucket with no empty way before it, distinct stamps, sound records)
and refuses bad bytes with a
`DecodeError` that names what is wrong; a different slot count re-places every record in stamp
order, so a smaller table keeps the newest of each bucket. The bytes take the place of the
daemon's `state/estimates` file beside its image (`state/history`), written when it writes
the image and at stop (request 2). Bytes of an older version give an empty history with a
note; any other bad history stops the daemon naming the file.

### Feeding the plane

The narrowest hook (request 1, written here since core is done): `blaze_graph::admission::
Prediction { cost: Option<u64>, peak: u64 }` with `apply(&mut Spec)`, and a trait `Predict:
Send` with `predict(kind, key)`, `observe(kind, key, ran_ns, peak)` and `encode()`.
`Estimates` implements it (cost `None`, today's behaviour), so a plane without history is
unchanged. `History` implements it with every cost in
nanoseconds, a seen key's from its record and an unseen key's from its kind, so costs on one
plane are of one unit. The cargo daemon's plane and the exec scheduler hold a `Box<dyn
Predict>`, apply each node's prediction to its `Spec` (memory is the larger of the spec's and
the prediction's, cost the prediction's when it has one), and observe each finished node's own
run time and peak (request 2).

### Tests (`crates/blaze-history/src/tests.rs`)

The median and peak rules, the median of mins and maxes against a sort over 100,000 random
runs and every order of four runs with ties; predicting (a seen key, an unseen key, no key, an
unseen kind), a reader's read and observing held to zero allocations; a record after an empty
way refused; a model of per-bucket LRU against the table over 100 random
streams, the reader seeing what the writer sees; encode/decode roundtrips, resizing both ways,
every `DecodeError`; byte-for-byte determinism; three readers racing 200,000 writes and never
seeing a torn record; the plane on a known graph (a single 2,000 µs node runs before a chain of 100 µs then 900 µs,
1,000 µs in all, where flat costs run in insertion order, the chain's head first); 200 random
graphs whose run order on one core matches a reference critical-path simulation; admission
holding two 80-byte keys apart and letting two 30-byte ones run together under a 100-byte
budget.

### State of the art

- **Seqlock-read tables** (the Linux kernel's `seqcount`, Folly's and crossbeam's seqlocks): the
  same protocol, in safe Rust over atomics, with one writer by type.
- **Set-associative LRU**: CPU caches and Caffeine/Ristretto-style bounded maps; ours is a fixed
  64-byte record per way, so memory is known up front and a lookup touches one bucket.
- **Scheduling from history**: Bazel and Buck2 record action durations and report the critical
  path after a build; cargo orders units by their dependents in the unit graph, with no timing;
  rustc's `-Zself-profile` records query times but nothing schedules by them. blaze schedules
  every query node, not every crate, by the critical path of its predicted times, and admits it
  by its predicted peak memory.
- **Not beaten yet**: remote execution's scheduling across machines (Bazel, Buck2); blaze's
  plane is one machine's.

## 12. blaze built with PGO and BOLT (`tools/pgo`)

`tools/pgo/pgo.py` takes one typed input (AGENTS.md rule 17, frozen dataclasses through
`typed_input`, unknown fields refused) of kind `build` or `publish`. `build`, in one ix VM per
target in the release train:

1. fetches the pinned LLVM (`crates/hir-ty-codegen/llvm.json`) through
   `.github/scripts/llvm.py`, which must also ship `llvm-profdata`, `llvm-bolt`, `merge-fdata`
   and `perf2bolt` and the profile runtime (request 5);
2. builds blaze (`-p blaze-engine --profile dist --features llvm`) with blaze, instrumented
   with `-Cprofile-generate` through `CARGO_ENCODED_RUSTFLAGS` (request 2 makes blaze read the
   flag, requests 3 and 4 make the LLVM backend and the linker honour it);
3. runs that blaze's daemon over `tools/pgo/training.json`: blaze itself at HEAD, ripgrep 15.1.0
   and nushell 0.108.0 at the commits `harness/bench.py` pins, and every crate of
   `harness/v2/corpus.json` (each `.crate` checked against the sparse index's sha256), with cold
   builds, no-op builds, edits (`const _: u64 = n;` appended, which changes the crate's code and
   fires no lint) and tests; every turn must succeed;
4. stops the daemon, waits for every process of the instrumented binaries to exit (by `pgrep`
   on their directory, never a signal to another lane's process), merges the raw profiles and
   refuses one under 10^9 counts;
5. builds blaze again with `-Cprofile-use` (and `--emit-relocs` on ELF, which BOLT needs);
6. on ELF targets runs BOLT on `blaze-engine`: trained by BOLT's own instrumentation by default
   (an ix VM has no LBR), or by `perf record -j any,u` and `perf2bolt` where the CPU has branch
   stacks, then `llvm-bolt -reorder-blocks=ext-tsp -reorder-functions=cdsort -split-functions
   -split-all-cold -split-eh -icf=1`. BOLT rewrites ELF only: asking for BOLT on Mach-O or PE is
   refused, and so is an ELF target that asks for none, so no Linux release ships without it;
7. runs the gate (by default `harness/ci.py run '{"part": "fast"}'`) with `BLAZE_BIN` set to
   the optimised engine;
8. times the turns of `training.json`'s `measure` projects (ripgrep) with a plain build of the
   same commit (`baseline`, or built here with no profile), the PGO build and the PGO and BOLT
   build, two rounds in rotated order, keeping each one's least time, and refuses a release
   that saves under `min_speedup` (5%) of the plain time: a profile that applied to nothing
   stops the release instead of shipping no speedup;
9. writes the profile, the BOLT profile and a deterministic `tar.xz` of the binaries into `out`,
   each named by its sha256, and `<target>.json`, its entry for `tools/pgo/profiles.json`, with
   the measured speedup (`speedup`: the plain time and the thousandths saved by the profile and
   by the profile and BOLT), which every `blaze` entry must name.

`publish` checks each named file against its sha256, uploads them to the release the pins name
(`pgo-profiles`) and writes the entries; a new toolchain drops every other target's profile. An
entry of the old cargo-trained pipeline reads as `builder: "cargo"`; a `blaze` entry also names
the LLVM commit that made it (an LLVM reads its own profile format only), its BOLT profile and
its binaries.

Tests (`tools/pgo/test_pgo.py`, `uv run --frozen python -m unittest tools/pgo/test_pgo.py`):
input parsing (defaults, unknown, missing and mistyped fields, BOLT per object format, gates
and counts), the checked-in `training.json` and `profiles.json`, pins roundtrip and merge,
content-hash naming, outputs checked against their files, deterministic packing, the profile
count, the encoded flags, the stage flags, the binaries of a manifest, the corpus index and
checksum, unpacking that refuses entries outside the crate, the measured projects named, the
speedup's arithmetic and floor, rotated rounds, a baseline holding both binaries, and a
`blaze` pin refused without its speedup.

### State of the art

- rustc's own dist builds (`src/tools/opt-dist`): PGO of rustc and LLVM, BOLT of LLVM on Linux,
  trained on rustc-perf's benchmarks. Clang's and Chromium's releases: PGO plus BOLT or
  Propeller. tools/pgo matches that pipeline (instrumented build, training on real workloads,
  merge, optimised build, BOLT, gate) and trains on the long-running daemon doing what users do:
  edits and no-op rebuilds as well as cold builds, which is where blaze's time is.
- **Beyond them**: every artifact is named by content and pinned with the LLVM that read it,
  so a release is reproducible from its pins, and the pipeline never builds blaze with cargo or
  rustc (AGENTS.md rules 1 and 2).
- **Not done**: Propeller (needs LBR or a newer LLVM's Propeller tooling on ARM), BOLT on
  macOS (no Mach-O BOLT) and Windows; context-sensitive PGO (`-fcs-profile-generate`) is a
  later step once plain PGO is measured.

## Requests (in `v2/requests/fastcompile.md`)

1. core: `Prediction`, `Predict` in `blaze_graph::admission`, `impl Predict for Estimates`
   (written here, with a test); left for core: exec's scheduler holds a `Box<dyn Predict>` and
   observes each job.
2. cargo: the daemon's plane takes a `Box<dyn Predict>`, observes each node's run, keeps node
   keys; the daemon persists `blaze_history::History` in `state/history`; `-Cprofile-generate`
   and `-Cprofile-use` read (an error until wired, never ignored) and their profile hashed into
   codegen settings.
3. codegen: `Spec.pgo` hashed into `Settings`; the LLVM backend's instrumentation and use passes;
   Cranelift with PGO an error.
4. link: the pinned LLVM's profile runtime linked into instrumented binaries; `--emit-relocs`
   honoured in ELF output.
5. coordinator: `llvm.py` ships the profile and BOLT tools; a release-train job per target runs
   `tools/pgo/pgo.py`; `flake.nix` and `build_release.py` take blaze-built pins; ruff, pyright
   and the scripts job cover `tools/pgo`.

## Speed of light

Nothing here was run: costs are counted from the code and the machine, as in
[speed-of-light-infer-relate.md](../db/speed-of-light-infer-relate.md), whose machine this uses:
an Apple M5 Max performance core at about 4.5 GHz, sustained IPC about 3.5, an L1 hit about 3
cycles, an L2 hit about 16 cycles (3.6 ns), DRAM about 100 ns; bandwidths are those of
[link's machine](../link/speed-of-light.md#the-machine) (one core from DRAM about 25 GB/s, a
page-cache copy about 10 GB/s). A mispredicted branch is taken as about 14 cycles.

### What waits on the history

- **A prediction per node.** `Plane::push` (`crates/blaze-cargo/src/serve/plane.rs`) asks
  `History::predict` for every node it adds, on the daemon's loop thread, inside the batch it
  builds; the plane is generic over its `Predict`, so the call is direct and inlined, with no
  `dyn`. It reads the table through the `Writer` it owns: relaxed loads, no lock, no write.
  Other threads read through `Reader`'s seqlock, which writes nothing shared.
- **An observation per finished task.** `Plane::poll` and `Plane::ended` call
  `History::observe` with the task's own run time and peak.
- **The image.** `History::encode` when the daemon writes its image, `History::decode` once at
  start.

A key is predicted once and observed once per build, minutes apart, while the compile workers
on the same cluster run through the shared L2. So a record's line has usually left L2 since its
key last ran, and the usual cost of a lookup is one miss to the system cache or DRAM. Both
cases are counted below.

### Unit costs

| step | cost |
|---|---|
| a key's bucket: its first word (no hash: a task key is a content hash), xor with the kind times φ, mask | about 7 cycles, after the key's load from the node (L1) |
| a bucket's four lines (256 bytes, two adjacent-line pairs), loaded at once | one L2 hit (16 cycles) or one DRAM miss (about 100 ns): the four loads are independent |
| compare four ways' kind, seen and key, and choose the way, no branch | about 30 instructions, 2 or 3 cycles of depth after the lines arrive |
| the chosen way's words (same line, now in L1) | about 4 cycles |
| median of the last four runs by min and max (`Record::ran_us`) | 6 operations, about 5 cycles of depth |
| a kind's Fx probe (hashbrown, a few hundred kinds, L1) | about 20 instructions, about 12 cycles of depth |
| a seqlock publish: odd sequence, release fence, seven stores, even sequence with release | about 12 instructions plus a `dmb ish` on arm64 (about 20 cycles; free on x86) |

### The bound and the code, per hot path

**A seen key's prediction.** The work that can't be skipped: find the key's bucket, read its
lines, compare the ways' tags (a set-associative lookup must compare before it knows the
way, as a CPU cache does, in parallel), take the median and the peak:

\[ T_{\text{hit}} \approx 7 + t_{\text{line}} + 3 + 5 + 3 \ \text{cycles} \approx 34\ \text{cycles} \approx 7.5\ \text{ns (L2)}, \quad \approx 103\ \text{ns (DRAM)}. \]

The code before this pass was about 65 cycles (14 ns in L2), almost twice the bound. Two branches
cost the difference:
- Its way scan returned at the way that held the key. When hot keys sit in every way of full
  buckets, that exit branch mispredicts.
- Its median insertion-sorted the four run times, which also mispredicts on run times that
  vary.

The code now (`table.rs`):
- `Writer::get` compares all four ways with `&` of booleans and a select, then loads the
  chosen way.
- `Record::ran_us` is min and max only, held to a sort-based reference by
  `the_median_of_mins_and_maxes_is_the_sorted_one`.

That is about 36 to 38 cycles, the bound plus the dependent L1 load of the chosen way (a select
tree over every way's words would save about 2 cycles and add 20 loads): within 6 to 10% in L2
and about 1% from DRAM.

**An unseen key's prediction, or a keyless one** (a cold build): the bucket read and compared as
above, finding nothing, then the kind's time and peak. The bound is the bucket and one kind
probe, which are independent, so their latencies overlap: about 30 cycles. The code probes two
maps (`Times` and the plane's `Estimates`). Both probes run in the shadow of the line load once
the miss is predicted, so a single lookup costs within a few cycles of the bound. In a batch of
back-to-back pushes, where the out-of-order core overlaps successive nodes, it costs about 20
instructions more than the bound's 50, 10 to 15% over. Request `fastcompile-sol-sched 1` (core:
`Estimate::observe` public) lets blaze-history keep one map of `(Time, Estimate)` per kind and
close it. That one map is the only change left.

**An observation.** The work that can't be skipped: read the bucket, choose the way (the key's,
else the first empty, else the oldest by stamp), update the record (ring slot, seen, stamp, the
peak rule), publish it so that readers never lock, and update the kind's mean and peak:

\[ T_{\text{obs}} \approx 7 + t_{\text{line}} + 5 + 4 + 20_{\text{fence}} + 3 \ \text{cycles} \approx 55\ \text{cycles} \approx 12\ \text{ns (L2, arm64)}, \]

and about 35 cycles on x86, whose fence is free. The kind's update is independent of the
record's and overlaps it.

The code before this pass chose the way with branches on each way's stamp, which mispredict in
full buckets. `Table::way` now chooses it with selects, the stamps' minimum included, so only the
publish's fence and the loads remain. That leaves the peek of the chosen way (about 4 cycles) and
the second kind probe (about 20 instructions, mostly hidden behind the fence): about 60 to 65
cycles, 10 to 18% over in L2 and a few percent from DRAM. Request `fastcompile-sol-sched 1`
removes the probe and leaves about 7%.

**Allocation.** None on any of these paths: `predicting_and_observing_allocate_nothing` holds a
seen key's, an unseen key's, a keyless and an unseen kind's prediction, a reader's read, and
observations of a seen kind (a seen key, a new key, no key) to zero allocations.

**The image.** Encoding reads the table (4 MiB) and writes each occupied record's 64 bytes. With
a full table the bound is about \(4\ \text{MiB} / 25\ \text{GB/s} + 4\ \text{MiB} / 10\ \text{GB/s}
\approx 0.6\) ms. The code before this pass:
- collected every record into a vector, about 3.6 MB written and read again;
- then wrote 13 fields per record through `put_*`, about 40 ns a record, about 2.6 ms in all:
  four times the bound.

It now streams each record as one 64-byte block built in registers into the buffer reserved for
the whole table, and patches the count after. That runs at the copy's bandwidth, within about
10%.

Decoding must check the untrusted bytes: each record's order, bucket, no gap before it, and
soundness, plus distinct stamps, which takes one radix sort. Then it fills a new table. The
bound is the read (0.4 ms for 4 MiB from the page cache), the checks (about 3 ns a record),
the sort of 65,536 `u64` stamps and the fill. It came within 10% of that once:
- each record is read as one block;
- the sort keys on the stamp alone (stable, so equal stamps keep slot order), not a 128-bit
  pair;
- `Writer::put_all` and `place_all` fill the table, which no reader shares yet, with plain
  stores, not 65,536 seqlock publishes and their fences.

About 1 ms once per daemon start.

**Bytes per lookup.** A lookup reads 256 bytes where the record alone is 64, the price of
keeping each way's tag in its record so that every way is compared in one miss. A Swiss-table
layout (a line of tags, then the record) reads 128 bytes but pays two dependent misses.
`Plane::push` does a node's other work (its name, its deps) between predictions, so few
lookups overlap and latency, not bandwidth, is the bound. 100,000 keys a build read about 26 MB
of history, about 1 ms of one core's bandwidth over the build. Which layout is faster for the
corpus's builds is measured once the lane is built, by the plane's own `Plane::stats`.

### State of the art

- **The lookup**: a CPU cache's set-associative lookup, in software, with every way's tag
  compared at once and no branch on the way. The same constant-latency choice Folly's
  `AtomicHashArray` and the kernel's seqcount readers make: no lock, nothing written on a
  read.
- **The median**: a sorting network's min and max, branchless, as in SIMD median filters.
- **Beyond Bazel and Buck2**: they record action durations and report critical paths after a
  build. blaze predicts every query node's cost and peak before it runs, at a few nanoseconds a
  node, and schedules by them.

### PGO and BOLT: the bound is the speedup available

What the literature measures for compilers:

| what | measured | where |
|---|---|---|
| rustc's Rust part built with PGO | 10 to 16% shorter real-world builds (about 5% fewer instructions) | [Inside Rust, Nov 2020](https://blog.rust-lang.org/inside-rust/2020/11/11/exploring-pgo-for-the-rust-compiler/) |
| Clang built with PGO | up to 20% (Clang's documentation); Firefox's builds up to 9% shorter | the same post |
| BOLT on rustc's LLVM, on top of PGO | 2 to 5% fewer cycles, 4% less peak memory | [Kobzol, Oct 2022](https://kobzol.github.io/rust/rustc/2022/10/27/speeding-rustc-without-changing-its-code.html) |
| BOLT on `librustc_driver`, on top of PGO and LTO | 2% mean wall time | Rust 1.75 release notes |
| BOLT on Clang 7, on top of PGO and LTO | 15% shorter Clang build; up to 20.4% across Clang and GCC; 52.1% without PGO and LTO | [Panchenko et al., CGO 2019](https://arxiv.org/abs/1807.06735) |

A compiler is bound by its instruction fetch and branches (a large, branchy, front-end-bound
binary), which is why layout helps it more than most programs. For blaze the speedup available
is about 10 to 16% from PGO and 2 to 15% more from BOLT on ELF, roughly 12 to 30% in all, the
top of the range for the daemon's warm loop (small edits, front-end-heavy), the bottom for
codegen-bound cold builds.

To reach it, a pipeline must do what the measured ones did (each item is in `pgo.py`):
- **Train on the time.** Real projects doing what users do. Here that is cold builds, no-op
  builds, edits and tests on blaze itself, ripgrep, nushell and the corpus, so the daemon's
  warm path is in the profile.
- **One raw profile per process** (`%p-%m`). Kobzol found concurrent writers to one file lost
  counts (rust#97110).
- **Optimise on top of LTO and one codegen unit.** The figures above are on top of LTO; the
  `dist` profile is thin LTO with `codegen-units = 1`.
- **Profile BOLT on the PGO-optimised binary**, not the instrumented one: the two-step pipeline
  of the BOLT paper.
- **BOLT's layout passes**: ext-tsp block order, cdsort function order, hot and cold splitting,
  with `--emit-relocs` so functions can move. Branch-stack sampling (`perf -j any,u`, more
  accurate) where the CPU has it; BOLT's instrumentation where a VM has no LBR, as rustc's CI
  does.
- **Profiles of this commit**, made fresh each release. A stale profile's functions don't match
  and LLVM drops them quietly.
- **Measure the result**, new in this pass. LLVM drops a profile that does not match, and the
  backend's PGO (requests `fastcompile-sched 3` and `4`) could take no effect, without an
  error: a release shipped at 0% looks like any other. `pgo.py` now times the measured turns
  with a plain build, the PGO build and the PGO and BOLT build, and stops below 5% saved (half
  the low end of PGO alone, above the noise of a VM). The pin records both figures
  (`Pin.speedup`), so every release states how close it came, and a BOLT that gained nothing
  shows as `total` equal to `pgo`.

Not reached, and why:
- **No BOLT on Mach-O or PE**: BOLT rewrites ELF only, so macOS and Windows get PGO's share
  only.
- **No context-sensitive PGO**: Kobzol found mixed results for rustc, at one more build each.
- **No Propeller**: it needs LBR or newer LLVM tooling on arm64.

### PGO and BOLT: the pipeline's cost per release

Per target, with \(B\) one `dist` build of blaze-engine with LLVM, \(T\) one pass of the training
set at optimised speed, \(T_m\) one pass of the measured turns (ripgrep's six), and \(G\) the
gate. The overheads of instrumented runs are assumptions: \(k_i \approx 1.5\) to \(2\) for
LLVM's, \(k_b \approx 2\) for BOLT's.

| stage | cost |
|---|---|
| plain build, the baseline | \(B\), or none with `baseline` (request `fastcompile-sol-sched 2`) |
| instrumented build | about \(1.1B\) |
| training, instrumented | \(k_i T\) |
| merge and count | minutes |
| build with the profile | \(B\) |
| BOLT: instrument, train, rewrite (ELF) | \(k_b T\) plus a few minutes |
| gate | \(G\) |
| measure: 2 rounds of plain, PGO, PGO and BOLT | \(6 T_m\) (\(4 T_m\) without BOLT) |

\[ C \approx 2.1B + (k_i + k_b)\,T + G + 6T_m \ \ (+B\ \text{with no baseline}). \]

The bound for a pipeline trained by instrumentation:
- **two builds**: BOLT rewrites the optimised binary without a third;
- **two training passes**: the BOLT profile must come from the final layout;
- **the gate**;
- **the least measurement that proves a speedup**: the plain and the final blaze once each,
  \(2T_m\).

So the code is the bound plus \(4T_m\), two rounds against noise and the profile's share
measured apart from BOLT's.

An illustration (assumed, not measured): \(B\) of 25 minutes, \(T\) of 20, \(T_m\) of 3 and
\(G\) of 10. Then \(C \approx 52 + 75 + 10 + 18 \approx 155\) minutes, about 2.6 hours. The
bound is about 143 minutes, so the code is about 8% over it with a `baseline`. Without one,
the plain build adds \(B\), about 16%; the release train's own plain build is that baseline
(request 2).

For comparison, rustc's dist pipeline took about 1.5 hours on Linux in 2022, rebuilding LLVM
and rustc three times.

The pipeline would get cheaper with sampling instead of instrumentation, AutoFDO and Propeller
style: profile release \(N\)'s binary in use and optimise release \(N+1\) with it. That removes
the instrumented build and both training passes, leaving \(B\) plus BOLT. It needs branch
stacks (LBR, or BRBE on arm64), which an ix VM lacks.
