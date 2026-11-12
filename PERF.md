# PERF: `perf/speedup`

What this branch makes faster, by how much, how it was measured, and what it could not measure.
Everything here was run on the machine described below; nothing is projected.

## Summary

| Benchmark (`crates/blaze-bench/benches/dedupe.rs`) | base | this branch | change, paired | speedup |
|---|---:|---:|---:|---:|
| `dedupe/generic_chains` (32,768 functions, 9 rounds) | 91–102 ms | 67–71 ms | −27% to −30% | **1.4×** |
| `dedupe/generic_chains_keeping` (same, with `Originals`) | 146–163 ms | 103–116 ms | −29% to −33% | **1.4×** |
| `dedupe/long_chains` (32,768 functions, 65 rounds) | 344–367 ms | 56–67 ms | −82% to −84% | **5.5–6.2×** |
| `dedupe/floor/clone` (control: unchanged code) | 4.7–5.8 ms | 4.7–5.8 ms | −1.4% to +1.0% | 1.0× |
| `dedupe/floor/code_ids` (control: unchanged code) | 12.0–15.0 ms | 11.6–14.3 ms | −3.7% to −5.1% | ~1.0× |

Ranges are over three paired runs (seeds 1, 2, 3). Each number is the runner's **mean** time
per iteration. The runner reports mean, min, max and stddev, not a median (see Methodology).
The two `floor` rows run code this branch does not touch. Their up-to-5% "significant"
differences are the noise floor on this machine: a change under about 5% here is not one.

**This is not a 500× faster blaze.** It is a 1.4× to 6× faster code-dedupe pass (identical code
folding), one stage of blaze's codegen. It is not end-to-end edit-loop time; that was not
measurable here (see Limits).

## What changed

### 1. `blaze_codegen::dedupe::dedupe_keeping`: rounds look only at what changed

Identical code folding repeats until nothing merges. When `Vec<u32>::push` and
`Vec<i32>::push` fold, their callers can become equal one round later, and so on up a chain.
The old implementation made a full pass over the whole build in **every** round:

- it grouped every candidate's code ID in a fresh `HashMap<ContentId, Vec<InstanceKey>>`;
- it ran `functions.retain(..)` over every function;
- it called `rename` on every function, each one probing the round's merge map.

A build whose folds chain `n` deep therefore cost `n` passes over all `N` functions: O(n·N).

The new implementation does the same work only where something changed:

- **Grouping only fresh IDs.** After a round, every group of equal code IDs has exactly one
  member left (its *holder*). So a new group must contain at least one function whose ID
  changed last round. A round groups only those IDs, each with its holder. The grouping is a
  sort of `(ID, key, position)` tuples, with no map of `Vec`s.
- **An index of callers.** Who names each function (relocations, data relocations, unwind
  personality or handler, exactly where `names_any` looks) is indexed once. Each function has
  a linked list of caller positions in one arena. When `B` merges into `A`, `B`'s list is
  spliced onto `A`'s in O(1), because those callers name `A` once renamed. A round renames
  only the callers of what it merged, and does so in build order.
- **Positions instead of key maps.** Code IDs live in a `Vec` by position. Merged functions
  are flagged and leave once, at the end, instead of a `retain` per round. The kept map is
  resolved once at the end instead of being rewritten every round.
- **A cheaper final sort.** The old code called `sort_by_key` on `Vec<CompiledFunction>`,
  whose elements are a few hundred bytes each and get swapped around by the sort. The new
  code sorts `(key, position)` pairs and moves each function once. This is the same order a
  stable sort gives. `Originals::functions` is sorted the same way.

A round now costs O(merged + renamed), and the whole fold costs O(N + total renames). That is
why the gain grows with chain depth: 1.4× at 9 rounds and about 6× at 65 rounds.

**Same answer, checked.** The old implementation is kept verbatim as `tests::reference`. The
new test `the_worklist_agrees_with_a_pass_over_every_function` runs both on 3,000 random builds,
each with and without `Originals`. The builds include calls, address-taken functions, vtables,
data naming earlier data, personalities, unmergeable and covered functions, statics, duplicate
keys, and data that is refused (`DataOrder`). The test requires these to be equal:

- the `Deduped` result (merges, rounds, bytes) and the renamed statics;
- the functions and statics;
- the originals;
- the error, and *which* error comes first.

On every successful run it also checks that no function still names a merged key.

`callers_taken_over_twice_follow_a_later_merge` covers a function that is kept for two merged
functions and then merged itself. Four deliberate bugs were planted, one at a time, to check
the tests catch them, and each one made the tests fail:

- dropping the holder from a group;
- renaming in reverse order;
- a stale list tail after a splice;
- ignoring duplicate keys.

### 2. `blaze-bench`: the store bench compiles again, and dedupe has more scenarios

- `benches/store.rs` no longer compiled on `main`: `Handle::put` now takes `&[u8]`.
- `benches/dedupe.rs` gains three things:
  - `generic_chains_keeping`: the driver's kept-rows path, with `Originals`.
  - `long_chains`: runs of 64, so 65 rounds.
  - Two floors: copying the build, and hashing one code ID per function. These bound what any
    fold can cost: about 18 ms of the 67–71 ms that `generic_chains` now takes.

## Methodology

- **Machine:**
  - CPU: an 8-vCPU Intel Xeon VM (cloud, shared; no SMT), with 16 GB RAM of which about 3.7 GB
    was free.
  - OS: Linux 6.12.113 (Debian 13).
  - Not isolated: other services ran on the box, which is why the noise floor is about 5%.
- **Toolchain:** `nightly-2026-10-27` (`rustc 1.101.0-nightly (16d1ffac1 2026-10-26)`), the one
  pinned in `harness/toolchain`. Both sides use `cargo bench`, the workspace's `release` profile
  and the same bench source.
- **Base:** `main` at `615873fa`, in a separate worktree, with this branch's
  `benches/dedupe.rs` and `benches/store.rs` copied in. The two binaries therefore differ only
  in `blaze-codegen`.
- **Paired runs:** `dedupe_new compare dedupe_old -t 20 -o --seed {1,2,3}`. This is
  blaze-bench's own paired runner:
  - Both binaries run as workers, and samples are taken in turns, in a seeded random order.
  - The statistic is the mean per-iteration difference, significant at |z| ≥ 2.6 (99%).
  - `-o` drops pairs outside Tukey's fences.
  - `*` in the raw output means significant.
- **Solo runs, for absolute numbers** (`solo -t 20`, min … mean … max, stddev). A noisy VM, so
  the means are higher than the mins:

  | | base | this branch |
  |---|---|---|
  | `generic_chains` | 70.6 … 80.4 … 219.6 ms (σ 16.4) | 44.1 … 50.4 … 138.3 ms (σ 7.0) |
  | `generic_chains_keeping` | 111.4 … 138.3 … 191.4 ms (σ 10.4) | 65.3 … 88.9 … 179.5 ms (σ 17.3) |
  | `long_chains` | 293.3 … 328.6 … 461.0 ms (σ 34.8) | 42.4 … 48.4 … 157.9 ms (σ 8.2) |

- **Profiling:** there is no `perf` or valgrind on this box, and neither is installable. So
  the phases were timed with temporary `Instant` probes. They are not committed. Before this
  change, about 75 ms of `generic_chains` broke down as:
  - setup (code IDs): ~20 ms;
  - grouping: 11–20 ms;
  - the rename pass over everything: 20–30 ms;
  - the final sort: 7–10 ms.

  After it, the remaining cost is mostly BLAKE3. That is ~13 ms of first code IDs plus ~9 ms
  of re-hashing 16k renamed functions, and both are needed for correctness.

## Tests run

- `cargo +nightly-2026-10-27 test -p blaze-codegen --lib dedupe::tests`: 14 of 14 pass,
  including the 2 new tests.
- `cargo +nightly-2026-10-27 test -p blaze-codegen --lib`, the whole crate:
  - **This branch:** 177 pass and 133 fail.
  - **Base:** 176 pass and 132 fail. The failure lists are identical apart from one test,
    `driver::tests::walk::the_oracle_catches_a_walk_in_the_order_its_describes_ran`. It fails
    on the base too when run alone (3 of 3 runs), so the base's one pass was a fluke.
  - **Cause of the failures:** 138 of the panics are blaze's thread pool refusing to start
    ("a thread's wait ring: registering its wait region: Invalid argument … **blaze needs
    Linux 7.3 or later, and this is Linux 6.12.113+**"). The other 3 are flight tests whose
    threads died of that same pool error.
- The same kernel limit makes these fail on base and branch alike:
  - `blaze-link`'s ICF tests (`tests::icf::*`);
  - the codegen driver's dedupe tests (`driver::tests::kept::…dedupe…`,
    `driver::tests::pieces::…dedupe…`).

  **Those integration tests were not exercised by this change.** Its correctness rests on
  the reference-equivalence test above. They should be run on a Linux ≥ 7.3 or macOS machine
  before this lands.
- blaze's own lints (`blaze check`) were not run, because they need a blaze binary, which
  needs Linux ≥ 7.3. The new code follows the house rules it could see: hashers are chosen
  with `blaze_hash`, and allocations carry `// alloc:` comments.

## Limits: why not 500×

- **blaze could not run end to end here.** Its runtime needs Linux 7.3+ (the io_uring-based
  wait ring in `blaze-core`, by design with no fallback). Building `blaze-engine` itself also
  needs the pinned static LLVM and more memory than this box had free. So the README's
  edit-then-build numbers (ripgrep 56 ms, helix 116 ms, bun 451 ms) were not reproduced, and
  this branch makes **no claim** about them.
- **Dedupe is one stage of a build.** How much of an edit's time it takes depends on the
  project. The `codegen dedupe` span in a profile shows it (`docs/v2/plan/design/icf.md`,
  "Measuring"). Making a stage that is X% of an edit 6× faster saves at most X% of that edit.
- **The rest of blaze is already tuned.** The query database's warm hit is about 5 ns
  (`db/hit/*`), and verifying an edge is about 2–5 ns. Hashing is BLAKE3 with cached domain
  keys. Release binaries ship with thin LTO, `codegen-units = 1` and PGO (`profile.dist`,
  `tools/pgo/`). There is no 500× left in any of these.
- **The headline numbers are already near their floor.** An edit takes tens to hundreds of
  milliseconds against cargo's seconds, and much of what remains is compiling the edited
  functions (Cranelift or LLVM), linking and writing the image. 500× faster than 56 ms is
  0.1 ms, less than a single process start or `fsync`.
- **What would move the edit loop further** (not done here):
  - parallelising the code-ID hashing on blaze's own pool, which is about 22 ms of the 67 ms
    left in `generic_chains`;
  - a profile of real edits on a Linux 7.3+ or macOS machine, to find the stage that
    dominates there.
