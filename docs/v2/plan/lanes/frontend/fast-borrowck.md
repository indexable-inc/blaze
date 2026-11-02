---
status: plan
summary: frontend-fast-borrowck — borrow checking's dataflow redone to its bound: rows chosen by size (dense up to 16 words, line-slotted above), sparse gen/kill, difference propagation in the fixpoint, one decoding pass into fact columns with a place-type column, cache before folding, every per-body buffer in the body's frame; NLL over the columns kept against a Datalog join.
date: 2026-10-29
---
# frontend-fast-borrowck: borrow checking at its bound

Section 8 of [fast.md](fast.md). The bound and today's counts are lane db's
[../db/speed-of-light-borrowck.md](../db/speed-of-light-borrowck.md) (called "the count" below);
this builds on it and does not recount what it counted. Machine figures: the top of
[../db/speed-of-light.md](../db/speed-of-light.md). Bodies: the count's typical (P 300, B 40,
N 120, M 160, L 25) and large (P 30 000, B 4 000, N 6 000, M 10 000, L 2 000) ones.

## 1. The work and its bound

The count's items 1 to 7 come to about 5 µs for the typical body, against about 180 µs of type
work. For the large body the dataflow is what is off: six gen/kill analyses (maybe
uninitialized and maybe initialized over move paths, ever initialized, use and drop liveness
over locals, loans in scope) each produce a set per block.

What any implementation must do for one analysis: compose each block's effect from its facts
(about P facts), and produce the entry set of each block that the walk reads. Only the words
that are not zero carry anything; a word that is zero in the output costs nothing if the output
says "zero" without storing it. So the bound per analysis is

- composing: P facts × ~20 instructions = 0.85 µs typical, 85 µs large;
- the output: the nonzero words of B rows, each written once and read once by the walk. For the
  large body's move paths, at most B × W_M = 4 000 × 157 words = 5 MB; a real row is runs of
  zeros and ones (the locals not yet live, then live), so most of its 64-byte lines are uniform.
  Taking half the lines as uniform, 2.5 MB written and read, 0.17 ms at 30 GB/s, and 0.5 ms for
  all six analyses.

The fixpoint itself, at its bound, sends each bit across each edge once: a gen/kill problem is
distributive, so the least fixed point is the union over paths, and a bit that has reached a
block never needs to be sent again.

## 2. What the code does today (counted from the code, 10-29 21:00)

- `dataflow::forward`/`backward` (`dataflow.rs`): every visit copies the entry row, applies the
  dense `GenKill` row (2W words read), and per successor copies `along`, runs the edge
  callback and unions (about 12 W words per visit). Large body: 313 MB moved, about 10 ms for
  the six (the count's figure).
- `GenKill` (`dataflow.rs`) is dense: B × W × 16 bytes, 10 MB for the large body's paths,
  zeroed per body, though a block's effect touches a few words.
- `BitMatrix` (`bitset.rs`) is dense and zeroed: 5 MB per path analysis, written in full.
- Passes over the statements: the count's nine (`Cfg::new`, `MoveData::gather`,
  `regions::build`, `liveness::effects_at`, `maybe_initialized_at_drops`'s composition,
  `Loans::gather` and its two-phase scan, `Loans::find_kills`, the walk) plus, in `loans.rs`,
  `static_locals`, `promoted_borrows`, `extended_temporaries`, `const_promoted` and the
  function-item scan, five more whole-body passes. Fourteen against the bound of one decode.
- Place types: `Ctx::place_ty` (`lib.rs`) projects from the local on every call; it is called
  from `moves.rs` (`gather_move`, `is_union_field`, `create`, `find`), `loans.rs`
  (`ignored`, `places_conflict`), 30 sites in `check.rs` and `regions.rs`'s own `place_ty`.
  About 4× the once-per-place bound (the count).
- `is_copy`/`drop_glue` (`lib.rs`): already look up the type as given before folding its
  regions, and store both keys (applied by db-ty-d-borrowck). Design 5 is done; it is kept.
- Every buffer is a global `Vec`/`FxHashMap` per body (`Facts`, `Csr`, `GenKill`,
  `BitMatrix`, `WorkQueue`, the scratch rows).

## 3. The design

### 3.1 Rows chosen by size (`rows.rs`)

A set of W words is stored one of two ways, picked once per analysis by W:

- **Dense**, W ≤ 16 (two cache lines; every typical body): B × W words, as `BitMatrix` was.
- **Lined**, W > 16: a row is a slot per 64-byte line (8 words), 4 bytes each. Slot 0 is a line
  of zeros, slot 1 a line of ones, any other slot names a line in a pool that grows as lines
  become mixed. This is the count's 2-bit summary per line (zeros, ones, mixed) with the mixed
  lines stored only when they exist, which is rustc's `ChunkedBitSet` (2048-bit chunks of
  `Zeros | Ones | Mixed`) at cache-line granularity. Zeroing the large body's path rows is
  then B × 20 slots × 4 B = 320 KB, not 5 MB, and a uniform line is never written or read: a
  line of ones absorbs any union without a load, and a reader (`load`) fills its row from the
  slot. A line only goes zeros → mixed → ones (the sets only grow), so a line is materialized
  at most once and a pool line left behind by "ones" is never reused.

The fixpoint is written once over a `Store` trait with the two impls, monomorphized: no match
per word. What the walk and liveness read is `EntrySets::load(row, &mut [u64])`, a dense copy
into an L1 buffer, the walk's own per-block read.

**Sparse gen/kill** (`rows.rs`, `Effects`): each row is a list of (word, gen, kill) triples
sorted by word, all rows one CSR (a `(start, end)` per row, so rows may be built in reverse
postorder). A row is composed into a dense scratch of W `[gen, kill]` pairs with a list of the
words touched, then flushed in word order and the touched words reset: O(touched), never W.
Applying a row is O(touched).

### 3.2 Difference propagation (`dataflow.rs`)

Each block has a pending row: the bits that reached its entry (forward) or exit (backward)
since it was last visited. A visit takes its pending lines, kills with the block's triples,
adds the block's gens on its first visit only, and sends the result over each edge; the
receiver keeps the bits that are new to it, adds them to its pending row and is queued if there
are any. Edge effects are data, not callbacks (`Edge::{Same, Nothing, Mask, Effect}`), applied
per line; an edge's own gens are sent on the sender's first visit only.

Exactness: every transfer here is `x ↦ (x & !k) | g` or `x ↦ x & m` or `x ↦ ∅`, each of which
distributes over union, so `f(a ∪ b) = f(a) ∪ f(b)` and the union of what is sent over time is
what the old algorithm sends on its last visit. Both compute the least fixed point, which is
unique, so the entry sets are bit-identical; the differential test checks it on every input.

Cost: a visit is O(lines of the row + lines with pending bits + triples), and each bit crosses
each edge at most once in a forward problem (at most once per successor that adds it, in a
backward one). Large body: about 1.5 edges × the output's mixed lines, all L1/L2-resident
pending and the entry line touched once, plus 20 slot reads per visit. That is the output bound
of 3.1 plus about 10% for the pending lines and the slot scans.

### 3.3 One decoding pass (`decode.rs`)

`Decoded::new` walks the reachable blocks in reverse postorder once and writes columns:

| column | for | today |
|---|---|---|
| moved operands (point, place) | `MoveData`'s moves | `MoveData::gather`'s walk |
| inits (point, place, deep or call return) | `MoveData`'s inits | same walk |
| `StorageDead` (point, local) | moves, ever-init, loan kills | three scans |
| uses, defs (point, local) | liveness | `effects_at` |
| drop candidates (point, local) | liveness (filtered by maybe-init) | `effects_at` |
| borrows (point, assigned place, borrowed place, kind, span) | loans | `Loans::gather` |
| writes (point, place) | loan kills | `find_kills` |
| two-phase uses (point, local) | activations | the second scan |
| statics, function items, `Ref` spans | `static_locals`, promotion | five scans |

Places in the columns are ids of the place-type column (3.4). Every analysis reads only
columns; the walk in `check.rs` and constraint generation in `regions.rs` still read the
statements (they report from them and type them; each is one pass of its own and the bound's
"walk" and "type work").

### 3.4 The place-type column (`places.rs`)

`Places` interns each distinct (local, projection prefix) of the body once: a parent id, the
element, and the `PlaceTy`, filled by one `projection_ty` from the parent's. A lookup is one
`FxHash` probe per element (about 10 ns) against a projection (about 100 ns with interning).
`Ctx::place_ty` goes through it, so every caller (moves, loans, the walk's 30 sites) gets each
place's type once per body with no call site changed. Move paths become a subset of place ids
(`MoveData` keeps its preorder numbering for the subtree ranges).

### 3.5 Cache before folding

Already in `lib.rs` (2). Kept.

### 3.6 The body's frame

Every per-body buffer of the files above comes from the body's task frame (`Ctx.frame`, the
design of `v2/requests/mem-db-checks.md` B1-B5: `FVec`, `FMap`, frame `BitSet`/`Csr`/`Facts`),
so after the first body warms the frame a check allocates nothing from the global allocator.
The new structures take `&'f Frame` from the start; the B5 signature table (what `regions.rs`
and `check.rs` call) is kept, so B6 and B7 apply as written but for the call sites listed in
the requests.

## 4. Polonius-style Datalog against NLL over the same columns

The columns are Polonius's input facts in dense ids (`loan_issued_at`, `loan_killed_at`,
`var_used_at`, `var_defined_at`, `var_dropped_at`, `path_moved_at`, `path_assigned_at`,
`subset_base`). Two ways to get the answers from them:

- **Datalog** (polonius-engine's `Naive`/`DatafrogOpt`): `origin_contains_loan_on_entry(o, l,
  p)` and `subset(o1, o2, p)` materialized per point, joined by sorted merges. Its tuples are
  up to live origins × loans × points: typical 250 × 25 × 300 / 100 (1% dense) ≈ 19 000 tuples,
  each sorted and merged at about 50 cycles, 0.27 ms, 50× the 5 µs the column NLL takes; large
  20 000 × 2 000 × 30 000 at 0.01% is 1.2 × 10⁸ tuples, seconds. rustc measured the same
  (polonius-engine about 10× NLL on `clap`, memory-bound), which is why `-Zpolonius=next` is
  a graph walk, not Datalog.
- **NLL over the columns** (kept): SCC region inference over interval sets, per-loan scope
  walks, the gen/kill fixpoint of 3.2. Each answer is computed once over dense ids, and the
  location-sensitive mode (`polonius.rs`, rustc's `-Zpolonius=next`) is the same graph walk over
  (region, run of points) nodes.

NLL is kept. There is no Datalog code in the crate to delete (`polonius.rs` is the graph walk).

## 5. What rustc behavior is kept, and how

- **Which programs pass, the codes, primary spans and their order**: the walk in `check.rs`
  is unchanged and reads the same entry sets, which 3.2 proves identical; the loans, their
  numbering (made in reverse postorder, statement order), their kills (`places_conflict` on the
  same places, in the same order) and their scopes are the same values from columns instead of
  statements. The order errors are found in is the walk's reverse postorder, unchanged.
- **Move numbering** (`rustc_move_indices`, the E0382 dedup across closures): moves and inits
  are recorded in the same order (reverse postorder, statement order, operand order).
- **Region variables and the place column**: projecting through an alias normalizes it, which
  may make inference variables. With the column, a place reached many times before
  `regions::build` (by the decode) makes them once, not per call, so fewer unconstrained
  variables come before the ones `regions::build` makes. That is an order-preserving
  renumbering: `regions.rs` types places through its own `place_ty`, not the column, so it makes
  the same variables in the same order; the variables made earlier are in no constraint (the
  columns keep places, not types); and `check.rs`'s `count` and `regions.rs`'s `first_new` are
  read after them, so every comparison between variables, and every value, is as before.
- **Liveness's normalization order** (the drop components found in rank order, which may make
  region variables): unchanged code over the same ranges.

## 6. Proof

- `differential.rs` (test only): the old `dataflow.rs` fixpoint and dense `GenKill`, copied,
  against the new `dataflow.rs` on the random graphs of the old tests and on generated large bodies
  (2 000 to 6 000 blocks, domains of 1 000 to 12 000 bits, chains, diamonds, nested loops,
  call-return edges, masks), both stores (dense and lined) forced on each, bit for bit; and the
  old `MoveData::gather`/`effects_at`/`Loans::gather` scans against the decoded columns.
- The borrowck fixtures in `crates/hir/src/tests.rs` end to end (codes, spans, order).
- `harness/v2/frontend/cases/borrowck/` (child `frontend-fast-borrowck-cases`): 60 programs
  including the large-domain edges (over 1 024 move paths, locals and loans, 200-block loops).
- Allocation budget (`budget` tests): composing, solving and loading the six analyses of a
  generated large body allocates nothing from the global allocator once the frame is warm.
