---
status: plan
summary: speed of light for borrow checking one body (crates/hir-ty-borrowck) — MIR reads, the bitset dataflow, liveness, region inference, loans, Polonius and the type work — with the arithmetic, today's counted cost, and the design that reaches 90%.
date: 2026-10-29
---
# Speed of light: borrow checking one body (`crates/hir-ty-borrowck`)

Owner: db-ty-sol-borrowck, under db-ty. The crate's techniques against rustc's are in
[borrowck.md](borrowck.md). Fusing the seven whole-store scans is db-ty-d-borrowck's work
(dispatch task 14, `scan.rs`), counted here as its design.

**Nothing here was measured.** Every cost is counted from the code and the sizes of what it
reads and writes, times the machine figures at the top of
[speed-of-light.md](speed-of-light.md): 3.5 GHz, about 2 instructions per cycle on branchy
code, L1 4 to 5 cycles, L2 (1 MB per core on Turin) about 14, L3 about 50, DRAM 110 ns, and one
core streaming about 30 GB/s. The body sizes are assumed; `blaze-engine borrowck --timings`
prints the real ones (`timings::Counts`), and `proof`'s budgets time each phase once the tree
builds.

## The two bodies counted

| | points P | blocks B | locals N | move paths M | regions R | constraints C | loans L |
|---|---|---|---|---|---|---|---|
| typical function | 300 | 40 | 120 | 160 | 250 | 600 | 25 |
| large body (a derived `visit_map`, a long `async fn`, a test of 500 asserts) | 30 000 | 4 000 | 6 000 | 10 000 | 20 000 | 60 000 | 2 000 |

A dense row of a set over `n` bits is `W = ⌈n/64⌉` words. Typical: `W_M = 3`, `W_N = 2`,
`W_L = 1`. Large: `W_M = 157` (1 256 B), `W_N = 94` (752 B), `W_L = 32` (256 B).

A MIR statement is about 100 bytes. That figure is estimated from the layouts of
`StatementKind::Assign(StoredPlace, Rvalue)` with two `Operand`s and the span, since no size
assertion exists. A typical body is then 30 KB of MIR (L2) and a large one 3 MB (L3).

## The bound per operation

1. **Reading the MIR.** Each pass over the statements reads P × 100 B. One pass costs about
   1 µs typical and 100 µs large (3 MB / 30 GB/s). The bound is **one decoding pass** that
   writes every fact the analyses need into 12-byte `Fact` rows: moves, inits, uses, defs,
   drops, loans, kills, activations and constraints. Everything after that reads the columns.
2. **A gen/kill analysis** (maybe uninitialized and ever initialized over move paths, maybe
   initialized at drops, use and drop liveness over locals, loans in scope). The work is
   composing each block's effect from its facts, which is about P facts at about 20
   instructions each (1.7 µs typical, 170 µs large), plus producing each block's entry row.
   With dense rows the output alone is B × W words written once and read once by the walk.
   For the large body's move paths that is 4 000 × 1 256 B = 5 MB each way, **0.33 ms**. For
   the typical body it is 40 × 24 B, which stays in L1 and is free.
3. **Liveness to region values.** The bound is the use, def and drop facts (about 2P), plus one
   interval union per (local, region of its type) pair, each a merge of a few runs.
4. **Region inference.** Tarjan's SCCs over R + C, then one interval union per DAG edge. At
   about 10 instructions per node and edge that is (250 + 600) × 10 / 2 / 3.5 GHz ≈ **1.2 µs**
   typical and **90 µs** large.
5. **Loan scopes.** NLL needs one walk per loan over the blocks the loan reaches, at one
   `first_absent_in` binary search per block. Polonius alpha needs one walk per loan over the
   (region, run of points) nodes it reaches. That is rustc's `-Zpolonius=next` loan liveness,
   and the bound is the reached nodes plus edges, with O(1) work per step.
6. **Loan kills** (rustc's `kill_borrows_on_place`). For each assignment, test the loans of the
   destination's local against the place once. A test projects the loan's place type one
   element at a time, d − 1 projections for depth d. A projection is a `field_types` memo hit
   plus an instantiation (about 100 ns with interning), so the bound is
   Σ over (assignment, loan of its local) of (d − 1) projections, **once per body**.
7. **The walk.** At each point, replay that point's kills and gens, and test each access
   against the in-scope loans of its local. The bound is the facts plus one conflict test per
   (access, in-scope loan of the local).
8. **Type work** (constraint generation in `regions.rs`, `is_copy` and `drop_glue`, place
   types, drop components, the regions a local's type keeps live). The bound is each distinct
   relation, predicate and place type computed **once per body**, and each statement's types
   walked once. The solver's global cache decides the rest: about 50 to 100 cycles per hit, as
   in [speed-of-light-solver-glue.md](speed-of-light-solver-glue.md).

For the typical body, items 1 to 7 come to about 5 µs. Constraint generation alone is about
300 statements × 2 types × 5 nodes × 30 ns ≈ 90 µs, and proving about 30 calls' predicates is
about 90 µs more. **Type work is about 97% of a typical body's bound**, so the dataflow can't
be what keeps a typical body from 90%.

## What the code costs today

Counted from the code as of 10-29 20:30.

**The fixpoint (`dataflow::forward` and `backward`)** costs about 12 W words per block visit:
read the entry row, write `out`, read the gen and kill words (2 W), then for each of about 1.5
successors copy `along` and union it into the successor's row (5 W each). With about 1.3
visits per block (reverse postorder, loops visited again):

- typical: 12 × 3 words × 8 B × 52 visits ≈ 15 KB per move-path analysis, all in L1, **0.13
  µs**, against the 1.7 µs of composing the effects;
- large: 12 × 1 256 B × 5 200 visits ≈ **78 MB** per move-path analysis and 47 MB per
  local analysis. That is a 15 MB working set (entry plus gen/kill), past L2 and in L3. The
  six analyses over the large body (two over paths, three over locals, one over loans) move
  2 × 78 + 3 × 47 + 16 ≈ 313 MB, **≈ 10 ms** at 30 GB/s.

**Passes over the statements: 9 today.** They are `Cfg::new`, `MoveData::gather`, the
composition in `InitFlow`, `regions::build`, the facts in `region_liveness`, its
maybe-initialized composition, `Loans::gather` (twice when a body has two-phase borrows),
`entry_sets`, and the walk. Against the bound of 1, that is 9 µs typical and 0.9 ms large
spent re-decoding MIR.

**Loan kills** (fixed in this change):
- **Before:** they were computed twice, in `entry_sets` and again in the walk. Each
  `places_conflict` rebuilt every prefix's type from the local, d(d − 1)/2 projections for
  depth d. At depth 3 that is 2 × 3 = 6 projections per (assignment, loan) pair against a
  bound of 2, **3× off**; at depth 5, 2 × 10 against 4, **5× off**. `ignored` re-projected the
  same way at each dereference.
- **Now:** `Loans::find_kills` runs `places_conflict` once per point in `gather` and stores
  the kills in a CSR, `kills`. `entry_sets` and the walk replay it through `Loans::after`, a
  slice read. `places_conflict` and `ignored` carry the prefix type forwards (one `project`
  per element, as `multi_projection_ty` folds), and `ignored` stops at the last dereference.
  This matches the bound.

**Polonius loan liveness** (fixed in this change): each step of each loan's walk probed an
`FxHashSet` of the universal regions twice, once for its direction and once for its points,
about 20 ns each with the hash and a miss. It now reads a `BitSet` over the regions, one L1
load, so the walk is at its bound of O(1) per step.

**Call-return effects in `InitFlow`** (fixed in this change): two gen/kill matrices had a row
for every block though only calls use them. For the large body that was 4 000 × 157 × 16 B =
10 MB over paths plus 6 MB over locals, zeroed per body. Each matrix now has a row per
reachable call. `liveness::maybe_initialized_at_drops` has the same `on_return` matrix; that
fix is requested from db-ty-d-borrowck (below).

**Type work:**
- `Ctx::is_copy` and `Ctx::drop_glue` run `fold_regions` on every call. That re-interns any
  type with a region variable before the cache lookup, about 50 to 100 ns per call, though
  most calls ask about a local seen before.
- Place types are projected again from the local in at least four places: the move-path
  gather (`moved_place`, `create`), loans, the walk's conflict tests, and
  `regions::build`'s `place_ty`. That is **≈ 4× the once-per-place bound** on that part.
- The rest of constraint generation (relating, normalizing, proving) can't be counted further
  without `--timings`.

**The store scans.** There were seven whole-store passes over the expressions per body
(`matched`, `tested_at`, `union_literals`, `guard_borrows`, `scrutinee_of`, `guard_bindings`,
`statement_values`). db-ty-d-borrowck's `scan.rs` makes them one per owner, which is the
bound.

## How far off

| part | typical: today / bound | large: today / bound |
|---|---|---|
| fixpoint rows | 0.13 µs / ≈ 0 (L1), inside the composition's 1.7 µs | ≈ 0.75 ms / ≈ 0.7 ms of output, **≈ 1.07×** (was ≈ 10 ms, then ≈ 1.65 ms) |
| MIR passes | 9 / 1 (9 µs of a ≈ 200 µs body, 4%) | 0.9 ms / 0.1 ms |
| loan kills | was 3× to 5× (depth 3 to 5), now at bound | same |
| Polonius per-step lookups | was 2 hash probes, now 1 load | same |
| place types | at bound: one projection per distinct place (`places.rs`), a probe per element to look up | same |
| `is_copy` / `drop_glue` | at bound: the raw type is looked up before folding | same |
| store scans | 7 / 1 (fixed by task 14) | 7 / 1 |

Recounted 10-30 by sol-all-ty, from the code, nothing built:

- **Fixpoint rows.** Sparse `(word, gen, kill)` effects, line-slotted rows (`rows.rs`) and
  difference propagation (`dataflow.rs`) had landed, which took the ≈ 10 ms to its output plus
  one fixed cost: every visit read every line of its pending row (a slot swap, a 64-byte copy
  and a compare, ≈ 8 cycles), nothing pending or not. For the large body that is 5 200 visits ×
  (20 + 20 + 3 × 12 + 4 = 80 lines over the six analyses) × 8 ≈ 3.3 M cycles, **≈ 0.95 ms**, on
  top of ≈ 0.7 ms of output: ≈ 1.65 ms, 2.4×. Each block now keeps a bit per line that holds
  unsent bits (`Marks`), and a visit reads only those, plus on its first visit the lines its
  own and its edges' gens touch. What is left is one mask word per visit per analysis, 6 ×
  5 200 × ≈ 5 cycles ≈ 0.05 ms: ≈ 0.75 ms against 0.7, **≈ 1.07×**.
- **MIR passes.** The one decoding pass (`decode.rs`) landed, but the default path still
  scans every statement in about nine more places, most matching only the kind's tag:
  `regions::build` and its `aggregate_temps`, the walk, `capture_temps`, `index_spans`, the
  assignment counts behind `rustc_move_indices`, `used_mut`'s temporaries, `coroutines` and
  `inline_consts`. `static_muts` now reads the decode's constants (`Constant::mutable`)
  instead of scanning. Each remaining scan still streams the 3 MB of a large body's MIR,
  ≈ 0.1 ms, so the row stays ≈ 9×; folding the rest into `Decoded` is the fix, in files
  db-mem-borrowck's windows are editing.
- **Place types, `is_copy`, `drop_glue`.** Design 4 (`places.rs`, the place column) and design
  5 (`Ctx::is_copy` and `Ctx::drop_glue` probe the raw type before `fold_regions`) landed.

For a typical body the dataflow is within 10% of its bound because the composition dominates
it. The gap is in the type work: recomputed place types and folding before the cache. For a
large body, dense rows are about 15× off.

## What stands in the way

1. **Dense rows past L2.** `BitMatrix`, `GenKill` and every consumer read a row as `&[u64]`:
   the walk's `BitSet::load`, liveness's `entry.row`, the `InitAtExit` masks, and the edge
   callbacks' `&mut [u64]`. A row summarized by chunk needs those call sites changed. Most are
   in `check.rs` and `liveness.rs`, which db-ty-d-borrowck is editing now.
2. **Nine passes over MIR.** Each analysis decodes the statements itself, through
   `for_each_operand`, matches on `StatementKind`, and `ctx.place_ty`.
3. **No place-type column.** `PlaceTy` is recomputed by projection wherever it is needed.
4. **The global allocator per body.** `Facts`, `Csr`, `GenKill` and `BitMatrix` are fresh
   `Vec`s per body. [borrowck.md](borrowck.md) already lists the per-body bump arena.

## The design that reaches 90%

1. **Pick the row representation by size, as rustc's `MixedBitSet` does.** Keep dense words
   up to 16 words (two cache lines), which covers every typical body. Above that, keep a 2-bit
   summary per 64-byte line (all zeros, all ones, mixed) beside the dense words, and have
   `apply`, `union` and the row copy skip uniform lines. Rows come from fresh zeroed pages, so
   untouched lines never fault. Store `GenKill` sparsely: a block's effect touches a few words,
   so keep (word, gen, kill) triples in a CSR and apply them in O(touched). Then a visit costs
   the touched words plus the mixed lines, and a large body's analysis costs about its output,
   ≈ 0.7 ms for all six instead of 10.
2. **Propagate differences in the fixpoint.** A block's first visit sends its whole `out`;
   later visits send only `delta & !kill`, the bits that are new at its entry. This is exact
   for gen/kill transfers and for the edge effects here (gen/kill on a call's return, `fill(0)`
   and masks), because all of them distribute over union. Loop revisits then cost the bits that
   changed, not W.
3. **One decoding pass.** `Cfg::new` and a single walk over the statements write every fact
   column (moves and inits, uses, defs and drops, loans, activations, kills) and the place-type
   column. The analyses read only columns: 12 bytes per fact instead of a 100-byte statement
   decoded nine times.
4. **A place-type column.** Intern each distinct (local, projection) once per body, with the
   `PlaceTy` of each prefix, filled during the decoding pass. The move paths, loans, kills,
   walk and constraint generation read it. That removes the ≈ 4×.
5. **Cache before folding.** `is_copy` and `drop_glue` look up the raw type first and fold
   only on a miss, storing both keys. This is requested below.
6. **A per-body frame arena** from `blaze-mem` for `Facts`, `Csr`, matrices and scratch, so a
   body allocates nothing from the global allocator.

With these, a typical body is its type work plus about 5 µs. A large body's dataflow drops
from about 10 ms to under 1 ms, within 10% of its output bound once (1) and (2) land together.

## The state of the art it matches

- **Dataflow:** rustc's `rustc_mir_dataflow`. It uses `DenseBitSet` for small domains and
  `ChunkedBitSet` (2048-bit chunks tagged zeros, ones or mixed) through `MixedBitSet` for
  large ones, gen/kill transfers and a reverse-postorder `WorkQueue`. blaze already beats it
  on composition: each block's effect is composed once, and a move path's subtree is a word
  range. Design 1 matches its large-domain answer at cache-line rather than 2048-bit
  granularity. Design 2 is difference propagation, the standard incremental worklist for
  distributive problems.
- **Liveness and region values:** rustc's `SparseIntervalMatrix` of points per region and SCC
  region inference. `IntervalSet` and `inference.rs` match both.
- **Loan liveness:** rustc's `-Zpolonius=next` (location-sensitive loan liveness over the
  localized constraint graph), which `polonius.rs` ports.
- **The benchmark:** rustc's own `mir_borrowck` time from `-Zself-profile` on the same
  bodies, with rustc-perf's large-body crates for the large case. The bar is ≤ rustc per body.

## Applied in this change

- `loans.rs`: `Loans::find_kills` and `Loans::killed_by` record each point's kills once in a
  CSR (`kills`). `Loans::after(cfg, location, state)` replays them, and
  `Loans::entry_sets(body, cfg)` no longer needs the `Ctx`. `places_conflict` carries the
  prefix type forwards, and `ignored` projects only up to the last dereference.
- `polonius.rs`: the universal regions are a `BitSet` read per step, not an `FxHashSet`.
- `moves.rs`: `InitFlow`'s call-return gen/kill has one row per reachable call.

Equivalence: the recorded kills are produced by the same `kill_on_place` code the walk used to
run. They are stateless and depend only on the loans and the place. The incremental
projection is the same fold `multi_projection_ty` does. The borrowck fixtures in
`crates/hir/src/tests.rs` and the randomized dataflow tests in `dataflow.rs` check the result
end to end.

Requested from db-ty-d-borrowck, whose files these are now:
- `check.rs`: the two call sites of the new signatures.
- `lib.rs`: design 5 (cache before folding).
- `liveness.rs`: the per-call `on_return` rows, and the drop-order rank taken per run rather
  than per point.
