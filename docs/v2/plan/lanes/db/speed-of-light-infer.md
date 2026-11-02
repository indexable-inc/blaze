---
status: plan
summary: speed of light for type inference of one body in hir-ty (infer.rs and infer/, InferenceResult's columns, infer/persist.rs), per expression and per body, with the arithmetic, what the code costs today, what was applied (writeback fast path, sparse columns as presence bitsets, the type table read in place) and what still stands in the way.
date: 2026-10-29
---
# Speed of light: type inference of one body

Files: `crates/hir-ty/src/infer.rs`, `crates/hir-ty/src/infer/` (expr, pat, path, coerce,
closure, unify, op, cast, mutability, columns, persist).

Nothing here was run. Every cost is counted from the code, not measured, and `proof`'s budgets
measure them once the tree builds. The machine figures are the ones at the top of
[speed-of-light.md](speed-of-light.md): 3.5 GHz, an L1 hit in 4 to 5 cycles, L2 in 14, about 2
instructions per cycle on branchy code, and one core streaming about 30 GB/s.

Some costs belong to other docs and are only counted here as calls:
- method probing is items' (`method_resolution/`);
- a solver evaluation is about 40 cycles on a global-cache hit (the db-ty-port-infer-errors
  section of [speed-of-light.md](speed-of-light.md));
- relating two types is in [speed-of-light-infer-relate.md](speed-of-light-infer-relate.md);
- the fulfillment loop and deep normalization are in
  [speed-of-light-solver-glue.md](speed-of-light-solver-glue.md);
- a salsa memo hit (a signature, a path's resolution) is 5 ns at salsa's bound
  ([speed-of-light.md](speed-of-light.md), vendor/salsa).

One body's inference runs on one core, in order. Parallelism comes from inferring many bodies
at once, so every bound below is one core's critical path.

## The body the numbers use

A typical function body:
- E = 300 expressions, P = 60 patterns, B = 40 bindings;
- 30 method calls or generic paths;
- A = 60 expressions with adjustments;
- O = 150 obligations registered (about one per two expressions: where clauses of called
  items, `Sized` for arguments and locals, well-formedness of explicit arguments, operator
  traits);
- D = 40 distinct types in each type column, and about 100 nodes in the record's type table.

## Per expression

**Work.** Read the expression's row, dispatch on its kind, compute its type from its children
and its expectation (often one unification of two 4-byte handles), and write 4 bytes. The
expression's own obligations are counted under "per obligation".

**Bound.**
- Reading the row: the store's kind and payload columns, near-sequential in arena order. About
  1 line per 4 expressions, so about 2 cycles amortized.
- Dispatch: about 10 instructions, plus an indirect-branch miss on about 1 in 3 expressions
  (about 15 cycles each), so about 10 cycles.
- One unification: a union-find root lookup on each side (2 L1 hits) and a compare, about
  15 cycles.
- The write: one 4-byte store into a column that is already the right length, about 1 cycle.

That is **≈ 30 cycles, ≈ 9 ns per expression**, plus the interning of any type the expression
builds (about 45 cycles a node, from infer-relate's unit costs).

**Today.** Before an expression's own work, `infer_expr_inner` and `infer_expr_inner_here`
cost:
- `unwind_if_revision_cancelled`, about 10 cycles (two dependent loads to salsa's flag);
- `ensure_sufficient_stack`, about 5 cycles;
- `#[instrument]`'s callsite check, about 5 cycles (no compile-time level cap, see
  `v2/requests/db-ty.md`);
- `insert_type_vars_shallow`, `write_expr_ty` (an `ArenaMap` insert that may grow), and the
  `resolve_vars_with_obligations(ty).is_never()` test, about 15 cycles together.

That is about 35 cycles of overhead on top of the bound.

Coercion is where the per-expression cost goes. A coercion site (every call argument, `let`
with a type, each branch of `if` and `match`, return) costs:
- two `try_structurally_resolve_type`s;
- an outer snapshot;
- for a reference or ADT target, an inner snapshot, a fresh region variable and one root
  `CoerceUnsized` evaluation, then a rollback;
- then `unify_raw`: a third snapshot, the relation, and an `ObligationCtxt` with its own
  evaluation round.

That is about 400 to 600 cycles for a reference or ADT and about 150 for a scalar (whose target
`coerce_unsized` rejects at once, as rustc does). With about one coercion per two expressions
at about 400 cycles each, coercion and its snapshots are about 200 cycles an expression on
average. Each step is
rustc's (`rustc_hir_typeck::coercion`). The bound for the same work is about 150 cycles: one
relation, one cache hit, and snapshots costing a length capture each.

| | bound | today | off |
|---|---|---|---|
| per-expression glue | ≈ 10 cycles | ≈ 31 (was 45) | ≈ 3.1× |
| a coercion (ref or ADT) | ≈ 150 cycles | ≈ 340–540 (was 400–600) | ≈ 2.6× |

Recounted 10-30 by sol-all-ty, from the code, nothing built:
- **Per expression.** `infer_expr_inner`'s `#[instrument]` and `infer_expr_inner_here`'s
  `trace!(?expr)` are gone, about 5 + 4 cycles. The cancellation check runs only for
  expression ids divisible by 64, a test of about 1 cycle plus 10/64 for the check. Ids are
  dense and every expression is inferred, so a cancelled revision still unwinds within 64
  expressions, even in a block-free array literal. Against the 45 counted above, that is
  45 − 5 (the `instrument`) − 10 (the check) + 1.2 ≈ **31 cycles**. The `trace!`, about 4
  more, was not in that count, so it may be ≈ 27. What is left is
  `ensure_sufficient_stack` (5) and the type write and never test (15). Both are rustc's.
- **Per coercion.** Snapshots were already length captures of each storage
  (`snapshot/undo_log.rs`). What they still paid was `tracing`. `start_snapshot`'s `debug!`,
  the `instrument`s and `debug!`s of `rollback_to`, `commit_from`, `commit_if_ok` and `probe`,
  `InferenceTable::rollback_to`'s `instrument`, `coerce`'s info-level `instrument` and its
  `debug!`, and `coerce_unsized`'s `instrument` and `debug!` are gone. That is about 12
  callsite checks of about 5 cycles per ref or ADT coercion, **≈ −60 cycles**. The workspace
  sets no `tracing` `max_level` feature, which is a decision for Andrew. With one, the other
  callsites in `hir-ty` go too.

**What stands in the way.**
- The tracing callsite checks: a compile-time level cap for release, requested.
- The cancellation check, which needs to run only once per block or loop body, not per
  expression: a cancelled revision then unwinds a few hundred expressions later, still within
  microseconds.
- Snapshot cost in `next_solver/infer` (port-infer's): a snapshot should be a length capture
  of the undo log and the region constraints, about 5 cycles.

## Per obligation and the fulfillment rounds

**Work.** Register each obligation once. Evaluate it when a variable it is stalled on changes,
and once more at the end.

**Bound.** One 20-byte row push into `Pending`'s columns (about 5 cycles), plus one evaluation
per change of its inputs (about 40 cycles on a cache hit). For the typical body:
150 × (5 + 40) ≈ 7K cycles, **≈ 2 µs**, plus the evaluations that do real work (the solver's
cost).

**Today.** Registration is at the bound: the fast path proves a goal without storing it, and
`Pending` is structure-of-arrays. The rounds are where the cost is. `select_obligations_where_possible`
runs a full pass over every pending obligation:
- from `resolve_vars_with_obligations` whenever a type is still an unresolved variable after
  resolving. That includes the per-expression `is_never` test on every integer and float
  literal (an `IntVar` is a non-region variable);
- from each `try_structurally_resolve_type` on a variable (twice per coercion);
- from method probes on a variable receiver;
- twice in `infer_finalize`.

Each pass is about 10 cycles per still-stalled obligation (the solver glue's count). Obligations
stalled on nothing are evaluated again at about 40 cycles. With V ≈ 100 rounds and about 35
pending obligations, that is 100 × 35 × 10 ≈ 35K cycles, **≈ 10 µs**. That is 5× the bound
for obligations, and it grows as V × P: quadratic in the body.

rustc does the same rounds (`FnCtxt::resolve_vars_with_obligations`,
`try_structurally_resolve_type`). Skipping one is observable: code that tests whether a variable
is resolved yet (closure signature deduction from pending obligations, `structurally_resolve`'s
"type annotations needed", a method probe on `?T`) would see a different state. So the fix
must keep each round's result and make an unproductive round free.

**The design that reaches it.**
1. **An empty round is O(1).** Give `InferCtxtInner` a `u64` count of state changes that can
   unstall a goal: a type, integer, float or const variable instantiated or unioned, or an
   opaque type registered. Bump it on every such change, inside a snapshot or not, and on
   rollback. `FulfillmentCtxt` keeps the count it last reached a fixpoint at, and whether an
   obligation was registered since. If neither changed, the round returns at once. Identical
   inference state gives identical evaluations, so the result is the same. The only things
   skipped are the obligation inspector's callbacks, which an unproductive round only repeats.
   Done by db-ty-port-infer: `InferCtxt::state_changes()` and `FulfillmentCtxt::fixpoint` in
   `fulfill.rs`. Region variables are not counted, as in rustc's
   `ty_or_const_infer_var_changed`. Runs of literals such as `f(1, 2, 3)`, where only fresh
   variables are made between rounds, now cost one round. Counting where rounds come from,
   about 70 of the 100 find nothing changed. The other 30 cost 30 × 35 × 10 ≈ 10K cycles, so
   obligations and rounds together are ≈ 17K, **2.4× the bound**.
2. **Watch lists for the rest.** Map each variable's root to the pending obligations stalled on
   it, the two-watched-literal scheme of CDCL SAT solvers (MiniSat). A union or instantiation
   then marks only that variable's watchers dirty, and a round visits only dirty obligations.
   The total cost is the number of (variable change, watcher) pairs, independent of how many
   rounds run: the bound. This replaces `goal_remains_stalled`'s per-round check, so it belongs
   with the solver glue and goes in the same request.

## Writeback (`resolve_all`, once per body)

**Work.** Resolve every type the body wrote: about E + P + B ≈ 400 entries, plus adjustments,
method arguments and closure captures, about 550 in all. Each entry needs its variables
resolved, its non-rigid aliases normalized, and any unresolved variable reported. Most entries
need none of that.

**Bound.** One test of the type's cached flags (one load of its interned row, L1 or L2) and a
branch: about 8 cycles for an entry that needs nothing. 550 × 8 ≈ 4.4K cycles, **≈ 1.3 µs**.
Entries with variables or aliases add the fold and the solver's work, which is theirs.

**Before this pass.** `Resolver::handle_term` ran
`deeply_normalize_with_skipped_universes_and_ambiguous_coroutine_goals` on every type, alias or
not. For each one that meant:
- an `At` and a universes `Vec`;
- `deeply_resolve_ignoring_regions` and `has_non_rigid_aliases`;
- a fresh `FulfillmentCtxt` and a `try_evaluate_obligations` loop over nothing;
- `drain_stalled_obligations_for_coroutines` and `collect_remaining_errors`.

That is about 150 to 250 instructions, ≈ 100 cycles a type: 550 × 100 ≈ 55K cycles,
**≈ 16 µs, ≈ 12× off**.

**Applied now** (`infer/unify.rs`, `Resolver::NORMALIZE_FLAGS`). `handle_term` normalizes only
a term with one of three things in its flags:
- a non-rigid alias;
- an error, which would taint the inference context in `deeply_resolve_ignoring_regions`;
- an unresolved non-region variable.

It also normalizes everything when `renormalize_rigid_aliases()` is set, as normalize.rs does.
Writeback calls `resolve_vars_if_possible` on the whole value before folding it, so a term
without those flags is one that normalization gives back unchanged, with no goal, no error and
no side effect. The skip is exact.

A term that also has no region variable is returned before `DiagnoseInferVars` too: its own
test (`HAS_INFER | HAS_NON_REGION_ERROR`) would give it back unchanged. Today an entry that
needs nothing costs one flags test, ≈ 8 cycles: **≈ 1.3 µs, at the bound**.

**What remains.** Types repeated across entries are resolved once per entry, as rustc does.
Resolving each distinct type once would change how many times a trait error is recorded, so it
is not done. It only matters for entries with variables or aliases, which pay the fold anyway.

## Freezing the tables (`InferenceTables::freeze`)

**Work.** Turn what inference wrote into canonical columns: each type column
dictionary-encoded, lists in key order, sets as bitsets.

**Bound.** One pass per column: about 3 cycles a key for a dense column (a 4-byte read and
write), plus one probe of a small L1-resident table per type entry to dictionary-encode it.
About 5 cycles × 550 ≈ **0.8 µs**.

**Today.** `TyColumn::from_entries` builds a `Vec<Option<_>>` from the `ArenaMap`, which is
already dense, and probes an `FxHashMap` per entry, about 20 cycles: ≈ 400 × 25 ≈ 10K cycles,
≈ 3 µs, **3.5× off**, about 1% of a body's inference. CSR columns written in key order are kept
as they are (O(keys)). The fix is to dictionary-encode straight from the `ArenaMap`'s slots
with a 64-entry direct-mapped table indexed by the type's row bits. That comes after `proof`
measures it.

## The result's columns at rest

**Work.** What a body's result must hold: a type per expression, pattern and binding, and a
row for the few nodes that have a method, variant, item, field or adjustments.

**Bound** (information). About ⌈log2 D⌉ = 6 bits per typed node (400 × 6 bits ≈ 300 B), the
dictionaries (about 0.5 KB), and per present row its data: about 30 methods × 12 B,
60 adjustment lists × about 5 B, and so on. In all **≈ 1.5 to 2 KB** for the typical body.

**Before this pass.** Every sparse column was dense by key:
- `IndexColumn` held a 4-byte place for every key up to its last row: 1.2 KB for methods,
  about 0.6 KB for variants, 1.2 KB for associated items;
- `Csr` held an 8-byte range for every key: 2.4 KB for expression adjustments and 0.5 KB for
  pattern adjustments.

That is ≈ 6.2 KB of places, most of them `NONE`. With the 1.6 KB of type places, the field
column's 1.5 KB and the rows, a record was ≈ 10.8 KB, **≈ 6× off**. The same bytes sit in the
memo, in every record in the store, and in every read.

**Applied now** (`infer/columns.rs`, `Presence`):
- `IndexColumn` and `Csr` keep which keys have a row as a bitset, 1 bit a key, with a rank
  per 64-bit word made again from the bits on load and never stored.
- A row's place is `ranks[w] + popcount(word & below)`: two independent loads and a popcount,
  the same dependent depth as the old `at[key]` then `rows[place]`.
- `Csr` keeps one `u32` end per listed key instead of an 8-byte range per key. An empty list is
  still not an absent one.

The sparse places shrink from ≈ 6.2 KB to ≈ 0.45 KB, and the record from ≈ 10.8 KB to
**≈ 5 KB, ≈ 2.7× off**. The record's schema is version 3. `presence_places_agree_with_counting_the_keys_before`
checks every place, both iteration directions and the count against a naive count over a
`BTreeSet`. The existing tests check the columns against map models, and that a column comes
back from its parts and refuses parts it would not make.

**What remains.**
- `TyColumn`'s places are `u32` though D is almost always under 256. Storing them as `u8` (or
  `u16`) when the dictionary fits makes the type places 0.4 KB instead of 1.6 KB, at the cost
  of a width branch in `get`.
- `FieldColumn` is dense (a tag byte and a word per expression, 1.5 KB) though about 1
  expression in 10 is a field access. Two presences (named and tuple fields) take it to about
  80 B.

With both, the record is ≈ 2.3 KB, within about 1.3× of the information bound. Both are
changes to `columns.rs` and the record's layout, next in this component.

## Persist: encode (`to_record`, `Persist::write`) and decode (`Persist::read`, `to_result`)

**Work.** Encoding writes the record's columns and a type table of the distinct types they
name. Decoding checks the record and makes the result again: every type interned again in this
process, every column checked.

**Bound.**
- The bytes: 5 KB at 30 GB/s, ≈ 0.17 µs each way.
- The type table: one place per distinct value on encode (a probe and its words, about
  50 cycles each), and one interning per value on decode (about 45 cycles). 100 × 50 ≈ 5K
  cycles, **≈ 1.4 µs**. This dominates.

So **≈ 1.6 µs each way**.

**Before this pass**, decoding also copied the type table twice: a byte at a time through
`flat_map(to_le_bytes).collect()` (about 2 cycles a byte with the `Vec` growing, for about
2.4 KB), then into an `Aligned`. That was ≈ 1.4 µs, so decode was ≈ 3.4 µs, **≈ 2.1× off**.

**Applied now** (`infer/persist.rs`, `TableView`).
- A record read from a buffer reads its type table in place. The table is a `Slice<u128>`
  that `follow` checked, so its bytes are a 16-aligned range of the buffer, found from its
  offset with no unsafe code. `TyTableReader` checks only relative offsets and absolute
  alignment, so the view reads exactly as the copy did.
- Only an `InferenceRecord` that was never written (`as_ref`) copies, now with one pre-sized
  `extend_from_slice` per word.
- Test: `a_written_type_table_is_read_in_place_as_the_bytes_of_its_copy` checks the view is
  inside the buffer, aligned, and equal to the copy.

Decode today: ≈ 1.4 µs for the table, plus ≈ 25 column allocations (about 20 ns each, 0.5 µs)
and 0.2 µs of copies, ≈ 2.1 µs, **≈ 1.3× off**.

**Encode today.** Three copies of the type table:
1. its `Writer`;
2. `table_words`;
3. the record's `Writer`.

Plus the record's `Vec`s growing. ≈ 2 µs, **≈ 1.25× off**.

**What remains.**
- The decoded result's ≈ 25 boxed columns are each an allocation. With `blaze-mem` (not yet a
  workspace dependency) they come from the memo's revision arena, as one block.
- The table could be written straight into the record's `Writer`: its offsets are relative, so
  only its inner header is in the way. That changes `next_solver/persist.rs`'s table format
  (solver-sync's), for about 0.2 µs, so it waits for a measurement.

## Summary for the typical body

| component | bound | before | today | off today | removes the rest |
|---|---|---|---|---|---|
| per-expression glue | ≈ 3K cycles | ≈ 13K | ≈ 9.3K (10-30) | ≈ 3.1× | `ensure_sufficient_stack` and the `write_expr_ty` / never test, rustc's |
| coercions (≈ 150 sites) | ≈ 22K cycles | ≈ 60K | ≈ 51K (10-30) | ≈ 2.3× | rustc's coercion algorithm: the `CoerceUnsized` probe and the `unify_raw` round |
| obligations and rounds | ≈ 7K cycles | ≈ 42K | watch lists landed (`fulfill/watch.rs`), not recounted | needs `--timings` | — |
| writeback | ≈ 4.4K cycles | ≈ 55K | ≈ 4.4K | 1× | — |
| freeze | ≈ 2.8K cycles | ≈ 10K | ≈ 3.2K (dictionary-encoded from the slots, landed) | ≈ 1.15× | one frame-map probe per type |
| record size | ≈ 2 KB | ≈ 10.8 KB | ≈ 5 KB | 2.7× | `u8` type places, field presences |
| decode | ≈ 1.6 µs | ≈ 3.4 µs | ≈ 2.1 µs | 1.3× | arena-backed columns |
| encode | ≈ 1.6 µs | ≈ 2 µs | ≈ 2 µs | 1.25× | the table written in place |

The whole body, without method probing and the solver's own evaluations: bound ≈ 40K cycles
(≈ 11 µs). Before this pass it was ≈ 180K (≈ 51 µs, 4.5× off). Today it is ≈ 105K (≈ 30 µs,
2.6× off), with the writeback skip and O(1) empty rounds in. Most of what is left is coercion
snapshots, then the rounds that still run, both rustc's algorithm at rustc's cost. Watch lists
for those rounds are exact and requested.

## State of the art

- **The algorithm.** rustc's `rustc_hir_typeck` with the next solver: the same per-expression
  checks, coercion order and fulfillment rounds, kept so every program infers as under rustc.
  Matching rustc is the floor. Beating it comes from making unproductive rounds free and from
  the watch lists, which rustc's `FulfillmentCtxt` doesn't have (it checks `stalled_on` per
  obligation per round). The watch lists are MiniSat's two-watched-literal propagation.
- **Writeback.** rustc's writeback normalizes every node too
  (`rustc_hir_typeck::writeback::Resolver::handle_term`). The flags fast path is the same check
  rustc's `normalize_with_universes` makes inside, moved before the fulfillment context is
  built.
- **Columns.** Succinct rank as in Vigna's rank9 and the Jacobson rank directory: 1.5 bits a
  key with an O(1) rank. Dictionary-encoded type columns as in Parquet and Arrow's dictionary
  arrays. rustc keeps typeck results as `ItemLocalMap`s (hash maps per owner); these columns
  are smaller and are read without hashing.
- **Persist.** Read in place, as in Cap'n Proto and FlatBuffers. Only the types are interned
  again, since an interned handle is only valid in the process that interned it.
