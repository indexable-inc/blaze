---
status: plan
summary: frontend-fast-infer — inference within a body (fast.md section 4). Coercion by head dispatch (one snapshot or none where rustc's order can't fall back, the unsizing attempt skipped where no candidate exists or the same attempt already failed) and watch lists for stalled obligations (a round visits only obligations whose variables were written), each proved equal to rustc's order case by case, with the differential test, the cases and the allocation budgets that check it.
date: 2026-10-29
---
# frontend-fast-infer: coercion and fulfillment rounds

Area: [fast.md](fast.md) section 4. Counts build on
[../db/speed-of-light-infer.md](../db/speed-of-light-infer.md) (the typical body, its bound of
≈ 40K cycles and today's ≈ 105K) and [../db/speed-of-light-solver-glue.md](../db/speed-of-light-solver-glue.md).
Machine: 3.5 GHz, L1 4–5 cycles, about 2 instructions a cycle on branchy code. Nothing here was
run; costs are counted from the code.

## 1. Coercion

### Work and bound

About 150 coercion sites a body (every argument, typed `let`, arm, block tail, return). The
work of one is: read two kind tags, pick the rule, relate once, record the adjustments. Bound:
≈ 10 cycles of dispatch, one relation (≈ 15 cycles when the types are one ID, ≈ 45 a node when
a variable is instantiated), one snapshot only where a rule can fail after writing state (a
length capture, ≈ 20 cycles today with five union-find marks): **≈ 50–150 cycles**, ≈ 22K a body.

### Today (`infer/coerce.rs`, counted)

`InferenceContext::coerce` → `commit_if_ok(coerce)` (snapshot S0, ≈ 40 cycles start plus commit)
→ `coerce`:
- `!` and inference-variable sources are handled first;
- `commit_if_ok(coerce_unsized)` (S1): for a target whose kind can't be `CoerceUnsized`'s
  right side it returns at once (S1 costs ≈ 40 for nothing); for an ADT, reference, pointer or
  parameter target it makes a region variable and a type variable, relates them (`unify_raw`:
  snapshot S2, the relation, an `ObligationCtxt` round), and visits a proof tree of
  `Source: CoerceUnsized<?U>` (a full solver evaluation with proof-tree building, no global
  cache: ≈ 1–3 µs), then rolls back;
- then the pointer and function rules, each relating through `unify_raw` (S2 again, and an
  `ObligationCtxt` even when the relation made no goal); `coerce_from_safe_fn` adds S3.

A scalar or tuple coercion: S0 + S1 + S2 + an empty `ObligationCtxt` round ≈ 150–200 cycles. An
ADT or reference coercion: ≈ 400–600 cycles plus the proof tree. `ADT == ADT` coercions
(`let s: String = t.clone()`) pay the proof tree for an answer that is always no.

### Design: head dispatch (`infer/coerce/fast.rs`)

rustc's order is R1 `!`, R2 from a variable, R3 unsizing (in its own snapshot, may fall back),
then exactly one of R4 raw pointer target, R5 reference target, R6 fn item source, R7 fn pointer
source, R8 closure source, R9 subtyping. `Coerce::head` reads the two kind tags and returns
`Never`, `FromInfer`, `Unsize` (R3 must be tried) or `Direct` (R3 cannot succeed: the terminal
rule alone). Then:

1. **R3's rejections by kind run before any snapshot** (`unsize_rejected_up_front`): a variable
   target, rustc's list of targets that are never `CoerceUnsized`'s right side, `&str → &str`,
   the missing lang items, and the reborrow's mutability mismatch. All are pure; rustc runs them
   inside S1, which then rolls back nothing.
2. **R3 is skipped where no candidate can exist** (`unsize_has_no_candidate`). `CoerceUnsized`
   has no builtin candidate in the solver (no `SolverTraitLangItem` for it), so a goal
   `Source: CoerceUnsized<?U>` has only impl, param-env, alias-bound and object candidates and,
   with an error in the goal, the error candidate. With a source head that has no relevant impl
   (`Interner::for_each_relevant_impl`, the solver's own lookup), no `CoerceUnsized` clause in
   the (elaborated) param env, a source that is no alias, `dyn`, parameter or reference, and no
   error in either type, the root goal is `NoSolution` with no candidates, so `CoerceVisitor`
   breaks and R3 returns `Err` (the unsize attempt's error value is discarded). The target must
   have no aliases, so that relating it to `?U` registers no goal whose evaluation could leave
   anything behind a rollback (an overflow warning). Everything else R3 does is rolled back.
3. **R3's failure is remembered per body** (`UnsizeMemo` in `InferenceContext`, 2 × 32
   direct-mapped entries, 512 bytes, no allocation): keyed by both types with their region
   variables erased and by `use_lub`, stored only when both types have no non-region variable,
   no error and no alias, and only when `CoerceVisitor` did not read the table's sizedness
   (`type_var_is_sized`, the one input that is not a function of the types). With those, R3's
   outcome depends only on the two types modulo region variables, the param env and the typing
   mode (fixed per body): the new solver canonicalizes every region of an input to a fresh
   variable and region relations never fail in typeck. `&String → &str`, `&Vec<T> → &[T]` and
   `&Foo → &Foo` coerce many times in a body and pay R3 once.
4. **No root snapshot where no rule can fail after writing state outside its own snapshot**
   (`Head::needs_root_snapshot`). Every write of R1, R2, R4, R6–R9 and R5 with a non-reference source
   is inside `unify_raw`'s snapshot, or can't fail (`NeverToAny`, `Coerce` goals). An eliding is
   exact because a rolled-back inner snapshot followed by the outer one's rollback and the outer
   one alone end in the same state, and committed snapshots change nothing (path halving
   differs, roots don't). S0 stays for `Unsize` (R3 falls back) and for reference-to-reference
   (R5's autoderef writes outside its snapshots). `coerce_from_safe_fn`'s S3 goes: its callers
   always run inside S0, `may_coerce`'s snapshot, or `could_coerce`'s throwaway context.
5. **`unify_raw` of one type with itself is a no-op** (`SolverRelating::tys` and
   `LatticeOp::tys` return `Ok(a)` before touching anything when `a == b`, so no goal, no
   write): it returns without a snapshot. A relation that made no goal skips the
   `ObligationCtxt` (registering nothing and evaluating nothing returns nothing).

What stays rustc's: the rule order, the adjustments and their targets, the obligations and
their order, the region and type variables made (and their numbering after rollbacks), every
error value returned and every diagnostic. One thing does not: when R3 is decided up front
(point 2) or remembered (point 3), the `CoerceUnsized` evaluations it would have rolled back
are not run, so an `ObligationInspector` no longer sees them. Nothing in inference reads the
inspector; it is a debugging hook.

After: a scalar or same-ID coercion ≈ 20–60 cycles; a variable to a type ≈ 80 (one snapshot,
the relation); `ADT → ADT` without impls ≈ 100 (two salsa hits for the impl lookup, then
subtyping); a remembered reference coercion ≈ 150 plus R5's autoderef. Coercions ≈ 25K a body,
within ≈ 15% of the bound, the rest being R5's autoderef (rustc's).

### Prior art

Swift's constraint solver and Roslyn's conversion classification both decide the applicable
conversion rule from the type heads before any trial binding (Roslyn's `ClassifyConversion`
fast paths for identity and built-in conversions). rustc tries each rule in a snapshot.

## 2. Fulfillment rounds

### Work and bound

≈ 150 obligations a body, ≈ 100 calls of `try_evaluate_obligations`. Bound: each obligation is
evaluated once per change of a variable it is stalled on, plus once when registered; an empty
call is O(1). ≈ 150 × (5 + 40) ≈ 7K cycles a body.

### Today (`next_solver/fulfill.rs`)

An empty call is O(1) already (`FulfillmentCtxt::fixpoint`, `InferCtxt::state_changes`). A
call after any change runs `Pending::retain` over every pending obligation, each paying
`goal_remains_stalled` (≈ 10 cycles: one probe per stalled variable and sub-root), and compacts
the columns: ≈ 30 productive calls × 35 pending × 10 ≈ 10K cycles, plus the evaluations. That
is V × P, quadratic in the body.

### Design: watch lists (`next_solver/fulfill/watch.rs`, `next_solver/infer/touched.rs`)

`goal_remains_stalled(s)` is false exactly when one of:
- a variable in `s.stalled_vars` is no longer an unknown root of its table
  (`ty_or_const_infer_var_changed` reads only that variable's row);
- a variable in `s.sub_roots` is no longer a root of the sub-unification table;
- `s.opaques` is `Yes` and the opaque-type storage changed (its count, and the lookup table the
  erased rerun reads, change only with the storage).

Each of those rows changes only through `UnionFind::set`, the one write of a union-find table
(path halving moves only non-roots' parents, which never makes a root a non-root), and through a
rollback, which restores the state of the snapshot's start.

1. **The touched log** (`Touched`, one per union-find table: equality and sub-unification for
   types, int, float, const). `set` appends `(index, seq)`; a snapshot marks its length; a
   rollback truncates to the mark; when the outermost snapshot closes, or at a write outside
   every snapshot, the log is cleared once it holds 1024 rows and its epoch moves. A reader's cursor is `(epoch, length, seq of the last row
   read)`: it is valid while the epoch is the same, the length is no more than the log's and the
   row before it is the one it read, so a reader can't skip rows that a rollback replaced;
   otherwise the reader wakes everything. 8 bytes and ≈ 3 cycles a write.
2. **Watches** (`Watches`, in `Pending`). A pending obligation is either ready (a bit per row)
   or watched: on every variable of its `stalled_vars` (in its table) and `sub_roots` (sub
   table), and on the opaque storage when `opaques` is `Yes`, as intrusive lists per variable.
   Draining the logs moves a written variable's watchers to ready. A round visits the ready rows
   in row order, which is registration order.
3. **The round** keeps rustc's: each visited row is checked with `goal_remains_stalled` first
   and, if stalled, watched again; otherwise evaluated exactly as before (same inspector call,
   same error pushes, same recursion-depth count and overflow). After each evaluation the logs
   are drained, so a later row whose variable that evaluation wrote is visited in the same
   round, as rustc's single pass does. A row evaluated to `Maybe` is ready for the next round,
   where rustc checks it next. Finished rows are marked dead and the columns compacted once at
   the end of the call, rebuilding the watches from the kept rows' `stalled_on`.

Exactness: at every visit, every row whose `goal_remains_stalled` is false is ready (a row is
unready only after that check said stalled; every later write to a variable it watches is in a
log after the reader's cursor and is drained before the next visit; a rollback either restores
an earlier stalled state or invalidates the cursor). So the rows evaluated, their order, their
results and every side effect are rustc's. Rows registered, cleared or drained elsewhere
(`register_predicate_obligation`, `collect_remaining_errors`,
`drain_stalled_obligations_for_coroutines`) are new or forget the watches (`Pending::truncate`).

Cost: an empty round is the five cursor checks and the opaque count, ≈ 20 cycles, and
allocates nothing. A productive call costs its evaluations plus ≈ 5 cycles per (write,
watcher) pair and one compaction pass. Rounds ≈ 150 × (5 + 40) + 100 × 20 ≈ 9K cycles a body,
**≈ 1.3× the bound**, the rest being the per-call drain, which is shared by every
`ObligationCtxt` on the context.

### Prior art

MiniSat's two watched literals (a clause is revisited only when a watched literal is assigned);
CHR and constraint-store solvers (Frühwirth) waking delayed goals on variable binding; Prolog's
attributed variables (`freeze/2`). rustc's `FulfillmentCtxt` checks `stalled_on` per obligation
per round.

## 3. The body after both

| | bound | today | after |
|---|---|---|---|
| coercions | ≈ 22K | ≈ 60K | ≈ 25K |
| obligations and rounds | ≈ 7K | ≈ 17K | ≈ 9K |
| whole body (speed-of-light-infer) | ≈ 40K | ≈ 105K | ≈ 62K |

What is still more than 10% off is outside these files: the per-expression glue (≈ 35 cycles
an expression; the tracing callsite cap and the cancellation check per block, requested by
db-ty-infer) and `freeze` (dictionary-encoding from the `ArenaMap` slots, db-ty-infer's). Both
are listed in the report for their owners.

## 4. What proves it

- `infer/coerce/fast/tests.rs`, `new_and_old_infer_every_fixture_the_same`: every body of the
  fixtures (the hir inference fixtures' programs plus the coercion and obligation shapes of
  `harness/v2/frontend/cases/infer/`) inferred with the new coercion and rounds and with the
  old ones (the old fulfillment loop copied into `fulfill/watch/tests.rs`; the old coercion is
  the same rules with every elided snapshot, R3's prefix inside its snapshot and no shortcut,
  switched by a test-only flag at each point that changed) gives equal `InferenceResult`s,
  diagnostics in the same order included.
- `next_solver/infer/touched/tests.rs`: the log against a model under random writes,
  snapshots, rollbacks, commits and reads (64 seeds); the log stays under its cap, commits
  inside snapshots included; a reader whose rows a rollback replaced is told so; reading and
  writing after warm-up allocate nothing.
- `next_solver/fulfill/watch/tests.rs`: ready rows are visited in row order; a row woken below
  the visit waits for the next pass, as rustc's single pass does; draining a list wakes each
  watcher and reuses its nodes without allocating.
- Budgets (`blaze-alloc-count`): an empty round allocates nothing; a direct coercion (a type to
  itself, and a variable to a scalar after warm-up) allocates nothing.
- Not written: a test of `Head` against a model of rustc's order pair by pair, and a budget
  for a memo hit. The differential test covers both through the fixtures.
- `harness/v2/frontend/cases/infer/`: programs for every head and every shape where the fast
  path hands over, each with rustc's outcome.
