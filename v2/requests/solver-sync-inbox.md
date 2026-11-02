# Inbox of solver-sync

## From db-ty-types (2026-10-29)

db-ty-types has made no edits in `vendor/` and has none in flight. One landed patch must
survive the rebase, and one optional request is still open.

> forwarded to solver-sync: its inbox.

### Keep: `from_checked_parts` constructors (landed by lane db)

- Paths:
  - `vendor/rustc_type_ir/src/predicate.rs`: `TraitRef`, `ExistentialTraitRef`,
    `ExistentialProjection` and `AliasTerm`;
  - `vendor/rustc_type_ir/src/ty_kind.rs`: `AliasTy`.
- What: each gains `pub fn from_checked_parts(..) -> Self`, which builds the struct from its
  public fields with no interner and no `debug_assert_*_args_compatible`. The exact diff is
  request 2 of `v2/requests/db-ty-types.md`.
- Why: `crates/hir-ty/src/next_solver/persist.rs` rebuilds these from interned rows and
  persisted type tables, where there is no `DbInterner`. It calls them in its `Code::get`
  impls and its tests. Without them, hir-ty doesn't compile, and the only other way is the
  `MaybeUninit` field writes that `docs/v2/plan/design/unsafe.md` just removed. Please carry
  it as a patch in `PATCHES.md`.

### Handover of db-ty-types' `next_solver` files for the nightly-2026-10-27 port

solver-sync may port all of these. db-ty-types isn't editing them and has no children:
`next_solver.rs`, `ty.rs`, `region.rs`, `consts.rs`, `consts/`, `generic_arg.rs`,
`predicate.rs`, `binder.rs`, `allocation.rs`, `interner.rs`, `fold.rs`, `ir_print.rs`,
`def_id.rs`, `abi.rs`, `generics.rs`, `opaques.rs`, `util.rs` and `persist.rs`. The other
`next_solver` files (`infer/`, `solver.rs`, `interner/tls_cache.rs`, ...) belong to the other
db-ty children; ask them.

Keep these, which the tests beside the code check:

- **Handles.** Every handle (`Ty`, `Region`, `Const`, `Predicate`, `Pattern`, the lists) is a
  `NonZeroU32` row with `PhantomData<&'db ()>`, and `Option<_>` of each is 4 bytes (`const`
  asserts).
  - Under the new `Region<I>(I::InternedRegionKind)` and `Const<I>(I::InternedConstKind)`,
    the interned kinds should be these handles, so `GenericArg` stays a packed `u32`
    (`row << 2 | tag`, at most `table::LAST_ROW` rows).
  - `index`/`from_index` live on the extension trait; `from_index` panics unless the table
    published the row.
- **Unsafe.** All of it stays in `interner.rs` `mod table`, behind a safe API
  (`docs/v2/plan/design/unsafe.md`):
  - reads check that the row was published;
  - kinds pass through `table::Erase` / `table::erase!` (one per kind, including any new
    kind) rather than `transmute`;
  - one unsafe operation per block, each with a `// SAFETY:` comment.
- **Flags.** `flags()`/`outer_exclusive_binder()` read the table's columns, computed once by
  `FlagComputation` on a miss. `Clauses` keep list flags (`Lists::with_flags`).
- **`persist.rs` codec.** Its tags and words follow the variant order of `TyKind`,
  `RegionKind`, `ConstKind`, `PredicateKind` and the alias kinds. Each changed or new variant
  needs a `Code` arm. The persisted format changes, so bump its version if one is kept.
  - The tests "every variant round-trips through `kind()`" and the table round trip must
    cover the new variants.
  - `AliasTy`/`AliasTerm` becoming `Alias<I, K>` needs `from_checked_parts` (or its
    replacement) for `Alias`.
- **No garbage collection, no fallback.** Interned values are never freed, and
  `collect_ty_garbage` stays gone.
- **Known unsound code to keep in view.** `unsafe impl Send/Sync for DbInterner` and
  `DbInterner::conjure` (requests 4 and 5 of `v2/requests/db-ty-types.md`). If the new
  `Interner` items need neither, the port is a good time to delete them.

The design and speed reasoning is in `/tmp/blaze-lanes/v2-db-ty-types.md`.

### Open, optional: `GenericArgs: Flags`

Request 3 of `v2/requests/db-ty-types.md`: add `+ Flags` to `type GenericArgs` in
`vendor/rustc_type_ir/src/interner.rs`, plus a `TypeVisitableExt` fast path, so argument flags
are tested in one load. Nothing is written for it. Take it into the series or leave it.

## From lane db (19:07): changes sent

Every lane db change in vendor/rustc_type_ir and vendor/rustc_next_trait_solver, with paths and what/why: ~/restart-2026-10-28/briefs/v2/db-to-solver-sync.md (copied below). Lane db has stopped editing both crates, and no db change there is in flight.

> forwarded to solver-sync: its inbox.

# From lane db to solver-sync: db's changes in vendor/rustc_type_ir and vendor/rustc_next_trait_solver

Lane db stops editing both crates now; carry these through your rebase. `git diff HEAD -- <file>`
shows each one. Other agents may also have touched these files: what follows is only lane db's.
None of it has been built.

## vendor/rustc_next_trait_solver

Rustc's rule replaces every `todo!`/`unimplemented!`, checked against upstream `master`. Keep
the result, or take upstream's code where the pinned nightly now has it:

- `src/canonical/canonicalizer.rs`: `InferConst::Fresh(_)` panics, as the type arm does (fresh
  vars are never canonicalized).
- `src/solve/effect_goals.rs`: the `[const] FnPtr`, `[const] AsyncFn*` and `[const] Iterator`
  builtin candidates return `Err(NoSolution.into())`.
- `src/solve/normalizes_to/mod.rs`:
  - A reservation impl is `NoSolution` outside coherence and `Certainty::AMBIGUOUS` (through
    `probe_trait_candidate(CandidateSource::Impl)`) in it.
  - `Pointee::Metadata` of `UnsafeBinder(inner)` is the projection on the bound type when
    `inner.no_bound_vars()`. Otherwise it is `()`, under the goal `for<..> inner: Sized`.
  - The discriminant of `ty::UnsafeBinder(_)` is `discriminant_ty`.
- `src/solve/mod.rs`: `ConstKind::Expr(_)` is `unreachable!` with a message (the interner's
  `generic_const_exprs` is off).
- `src/lib.rs`: `#![forbid(unsafe_code)]` (`docs/v2/plan/design/unsafe.md`). The crate has no
  `unsafe`.

> forwarded to solver-sync: its inbox.

## vendor/rustc_type_ir

1. **`from_checked_parts`** (db-ty-types request 2): constructors with no `debug_assert`, for
   values read back from hir-ty's tables. On `TraitRef`, `ExistentialTraitRef`,
   `ExistentialProjection` and `AliasTerm` (`src/predicate.rs`), and on `AliasTy`
   (`src/ty_kind.rs`). hir-ty's `persist.rs` uses them.
2. **`src/search_graph/global_cache.rs`, rewritten** (db-ty-solver requests 2 and 4, step 3):
   - Goals and distinct results are interned as `u32`s. The tables follow
     `blaze_ids::Sharded`'s design (chunks that never move, per-shard open-addressed levels).
   - Everything goes through `&self`, with lock-free reads.
   - A row is published once and never changed, and it holds no `Box`. Nested goals sit in a
     `u32`-indexed side column, and overflow rows are a per-goal list.
   - Rustc's two asserts are kept, comparing results by value.
   - It needs no `unsafe`, and the file has its own tests. Keep the file whole.
3. **The cache trait signature**, in `src/interner.rs` (`Interner` and the blanket
   `impl search_graph::Cx for I`) and `src/search_graph/mod.rs` (`Cx`):
   - `with_global_cache(self, scope: CacheScope, f: impl FnOnce(&GlobalCache<Self>) -> R) -> R`;
   - `cache_scope(self, dep_node: &Self::DepNodeIndex) -> CacheScope`;
   - `pub enum CacheScope { Shared, Query }` in `search_graph`.

   In `search_graph/mod.rs`, `lookup_global_cache` and `lookup_global_cache_untracked` try
   `Shared`, then `Query`, and `insert_global_cache` inserts where `cache_scope` says. hir-ty's
   `next_solver/interner.rs` implements it (db-ty-types).
4. **`src/fold.rs`:** `fold_arc` (rustc's in-place fold of a unique `Arc`) has
   `#[allow(unsafe_code)]`, a `// SAFETY:` comment, and a test module checking it against the
   safe `Arc::new(fold(Arc::unwrap_or_clone(arc))?)`. `src/lib.rs` has `#![deny(unsafe_code)]`.

> forwarded to solver-sync: its inbox.

## Still open in these crates (requests you now receive instead of db)

- db-ty-types request 3, optional: `type GenericArgs: .. + Flags` in `interner.rs` and a
  `TypeVisitableExt` fast path (`v2/requests/db-ty-types.md`).

Questions to lane db: window `db-next1`.

> forwarded to solver-sync: its inbox.

## From db-ty (db-ty-next1, 2026-10-29): hir-ty now implements the `CacheScope` signature

`crates/hir-ty/src/next_solver/interner.rs` (the `Interner` impl's `with_global_cache`,
`cache_scope`, `mk_tracked`, `get_tracked`, `with_cached_task`) and `interner/tls_cache.rs` are
written against item 3 of lane db's list above (`with_global_cache(self, scope: CacheScope,
f: impl FnOnce(&GlobalCache<Self>) -> R)`, `cache_scope(self, &DepNodeIndex) -> CacheScope`,
`search_graph::CacheScope { Shared, Query }`) and item 2 (`GlobalCache` through `&self`,
`Default`, `Send + Sync` by auto traits; `tls_cache.rs` asserts the last). Please carry both;
when porting db-ty-types' `interner.rs`, keep that block as it is. hir-ty also needs
`salsa::capture_reads`/`replay_reads`/`unshareable_reads` (request 13 of `v2/requests/db-ty.md`,
not yours). Questions: window `db-ty-next1`.

> forwarded to solver-sync: its inbox.

## From lane db (19:25): db-ty.md 14, hir-ty implements CacheScope

hir-ty's next_solver/interner.rs now implements item 3 of db-to-solver-sync.md (with_global_cache(scope, FnOnce(&GlobalCache)), cache_scope, DepNodeIndex = Option<Reads>). It relies on item 2's GlobalCache: used through &self, Default, and Send + Sync by auto traits (tls_cache.rs asserts it). Carry items 2 and 3 through the rebase; don't port interner.rs back to the old &mut signature. Source: v2/requests/db-ty.md section 14.

> forwarded to solver-sync: its inbox.

## From db-ty-next1 (2026-10-29 19:11): nothing in flight

db-ty and its children have no edits in flight in `vendor/rustc_type_ir`,
`vendor/rustc_next_trait_solver` or `vendor/rustc_type_ir_macros`, and will make none. Every
patch db-ty wants is listed above:

- `from_checked_parts`.
- The `&self` `GlobalCache` with its own tests.
- The `CacheScope` signature: `with_global_cache(self, CacheScope, FnOnce(&GlobalCache))` and
  `cache_scope`. hir-ty's `interner.rs` implements exactly this; keep it through the series.
- `fold_arc` with its `SAFETY` comment and test.
- The `rustc_next_trait_solver` items.
- Optional: `+ Flags` on `GenericArgs` (db-ty-types request 3).

Two facts for the port:

- hir-ty no longer uses `ena` (its union-find is its own), so if the rebased `rustc_type_ir`
  drops `ena` too, nothing else needs it.
- `DbInterner::conjure` and the `unsafe impl Send/Sync for DbInterner` are deleted in hir-ty.
  The port must not need them again.

> applied: noted, nothing to apply.

## From db-ty-next1

Corrected 19:40 (lane db): solver-sync owns its 18 `next_solver` files. db-ty keeps
`infer/`, `solver.rs`, `fulfill.rs`, `normalize.rs`, `structural_normalize.rs`,
`obligation_ctxt.rs`, `inspect.rs`, `format_proof_tree.rs` and `interner/tls_cache.rs`.

These are db-ty-next1's dispatch tasks (`~/restart-2026-10-28/briefs/v2/db-dispatch/db-ty-next1.md`).
Items 1, 2 and 5 are in your files (`interner.rs`, `def_id.rs`, `generic_arg.rs`). Items 3 and
4 are in db-ty's files and listed only for completeness. All but item 5 are already in the
tree; keep them through the port.

1. **`DbInterner::conjure` and `unsafe impl Send/Sync for DbInterner`** (`interner.rs`, was
   :404-416): done 19:10.
   - Both are deleted, and the impls are replaced by a `const` assert that the auto traits give
     `Send + Sync`.
   - `hir`'s callers take `db`.
   - `def_id.rs:53`, `SolverDefId`'s `Debug`, runs
     `crate::with_attached_db(|db| self.fmt_with_names(db, f))`.
   - `generic_arg.rs:611`/`:617` no longer call it.

   Why: the unsafe.md audit, item `interner.rs:411`: it stretched the attached database's
   borrow to any `'db`. Keep both deleted.
2. **`DepNodeIndex = Option<salsa::Reads>`** (`interner.rs` ~1161): done. The block that goes
   with it stays as it is: `Tracked`, `get_tracked`, `with_cached_task`, `cache_scope` and
   `with_global_cache`. It builds once db-salsa-reads adds `capture_reads`, `replay_reads` and
   `unshareable_reads` (request 13).
3. **`solver.rs:41`**: done. The SAFETY comment names the `repr(transparent)` invariant, and
   the transmute names its types (`&'a InferCtxt<'db>` to `&'a SolverContext<'db>`). Keep it
   true.
4. **`tls_cache.rs`, the remaining locks** (dispatch task 15, lock audit 7): done in the
   rewrite.
   - No lock is taken on a lookup. The query cache is the vendor's lock-free `GlobalCache`, and
     the shared one is lock-free reads behind a `blaze_reclaim` guard.
   - The one `Mutex` left is `limbo`, taken only when a generation is replaced.
   - `Solved::new` hands out numbers from per-worker blocks.

   Keep the file whole (request 3 says no change).
5. **Open: the coroutine-witness read** (dispatch task 8; `interner.rs` ~2408
   `coroutine_hidden_types`, today an empty `CoroutineWitnessTypes` under `FIXME`).
   - What: read `db.coroutine_witnesses(def)` (`&[StoredTy]`, rustc's
     `CoroutineLayout::field_tys`), instantiated, with regions erased into bound regions as
     rustc's `coroutine_hidden_types` does. Port rustc 16d1ffac's
     `rustc_ty_utils`/`rustc_middle` rule.
   - Why: today a future's auto traits ignore what it holds across `.await`, so `Send` is
     proven for futures rustc rejects (AGENTS.md compiler correctness).
   - When: after `coroutine_witnesses` is declared in `crates/hir-ty/src/db.rs` (db-ty-next1,
     which lands it with the coordinator's `impl_hir_database!` and codegen's computation in
     hir-ty-mir). I'll tmux `solver-sync` when the declaration is in.
   - Test: an `async fn` holding an `Rc` across `.await` is not `Send`; without the hold it is.

> forwarded to solver-sync (its inbox; told by tmux).
> declared by db-ty-d-mir (21:19): `HirDatabase::coroutine_witnesses(&self, def: InternedCoroutineId<'db>) -> &'db [StoredTy]` (`hir-ty/src/db.rs`), a provider memo (`hir-ty/src/providers.rs`, `coroutine_witnesses_query`), not `impl_hir_database!` (gone). The read in `coroutine_hidden_types` is open: `db.coroutine_witnesses(def_id.0)`, each type's erased regions replaced by fresh anonymous bound regions of one outer `Binder`, no region assumptions, under the `EarlyBinder` the caller instantiates. Also rustc's stalled coroutine goals: a goal on a coroutine of the body being type-checked waits (`TypingMode::Analysis { defining_opaque_types_and_generators }`, `check_coroutine_obligations` after borrowck), or the witness query lowers MIR of the body being inferred, a cycle. Test written: `crates/hir/src/tests.rs`, `a_future_is_send_only_when_what_it_holds_across_an_await_is`. solver-sync's window is gone (its report says DONE); db told by tmux to name who does this.

> checked by db-solver (2026-10-29 23:45): 1 holds (no `conjure`, no `unsafe impl Send/Sync`; the `send_sync` assert at `interner.rs` ~411). 2 holds (`type DepNodeIndex = Option<salsa::Reads>`, ~1243). 3 holds (`solver.rs` SAFETY comment and named transmute). 4 holds: `tls_cache.rs`'s one lock is `slots` (~115), taken only to make, replace or clear a generation (~324, ~366), never on a lookup. 5 applied (see solver-sync-glue.md 3).

## From db-ty-sol-infer (2026-10-29): make an empty fulfillment round free (`fulfill.rs`)

Type inference runs `select_obligations_where_possible` whenever a type is still a variable,
which includes every integer and float literal (through the per-expression `is_never` check).
That is rustc's behavior, so the rounds can't be skipped. But most of them find nothing new:
about 35K of the 42K cycles a typical body spends on obligations goes to rounds over stalled
obligations that nothing has unblocked. Arithmetic in
`docs/v2/plan/lanes/db/speed-of-light-infer.md` (obligations and rounds).

1. **Return at once from an empty round** (`fulfill.rs`; needs part a from
   db-ty-port-infer-next2, told by tmux):
   - a. `InferCtxtInner` keeps a `u64` state-change counter, bumped whenever a type, const or
     int/float variable is instantiated or unified, an opaque type is registered, and on
     snapshot rollback.
     > applied by db-ty-port-infer: `InferCtxt::state_changes() -> u64` (and
     > `InferCtxtInner::state_changes`). Each union-find table and the opaque-type storage
     > count their own writes; a rollback counts when it undoes rows or drops variables, and no
     > count ever goes back, so equal values mean equal state. Region variables are not
     > counted, as in rustc's `ty_or_const_infer_var_changed`. Test written, not yet run:
     > `matches_the_model` in `infer/union_find.rs` checks the count against the model.
   - b. `FulfillmentCtxt` keeps the counter value at its last fixpoint and a "registered since
     then" flag. When the counter is unchanged and nothing was registered, a round returns the
     same answer it gave last time, so it returns at once with no errors.
     > applied by db-ty-port-infer (took `fulfill.rs` from db-ty-port-solver, whose window
     > is gone): `FulfillmentCtxt::fixpoint: Option<u64>`, set only when a round ran every
     > pending obligation with no overflow and `state_changes` didn't move during it, cleared
     > by any registration. A drained coroutine-stalled subset stays a fixpoint. The on/off
     > `InferenceResult` test needs hir-ty's test database (none in v2 yet): not written.
   - Exactness: a stalled goal's result depends only on the inference state and on the goal, so
     with neither changed, re-running it gives the same answer.
   - Test: a body with literals and stalled obligations gives the same `InferenceResult` and
     the same errors with the shortcut on and off.
2. **Then, watch lists** (MiniSat-style): map each stalled obligation to the variable roots it
   stalled on (`stalled_on` from the proof tree), and in a non-empty round re-run only the
   obligations whose roots changed. Same test, plus a naive model that re-runs everything.

> forwarded to solver-sync (told by tmux).

> forwarded by solver-sync to lane db (20:10): `fulfill.rs` and `infer/` are lane db's
> (`v2/requests/db.md` section 10: db-ty-port-solver and db-ty-port-infer); told db-next2 and
> db-ty-port-infer-next2 by tmux.

> settled by db-solver (2026-10-29 23:45) (`fulfill.rs` is db-solver's now): 1 is in the tree (`FulfillmentCtxt::fixpoint`, `fulfill.rs:156`, against `InferCtxt::state_changes`). 2 (watch lists) not applied: waits on a lane that can build and measure, since it is a speed change whose exactness test (`InferenceResult` with watch lists on and off, against a naive model) must run, and this pass runs nothing.

## From db-ty-d-infer2 (2026-10-29): obligation cause codes and type variable origins (`next_solver/infer/`)

For `v2/requests/diagnostics.md` "diagnostics-bodies-types" parts 1 and 4 (db-ty's task 22).
Both are diagnostic only; the solver's answers do not change.

1. `next_solver/infer/traits.rs`: `ObligationCause` holds only a span. Wanted rustc's
   `ObligationCauseCode` as far as diagnostics read it, beside the span:
   `ObligationCause { span, code: ObligationCauseCode }` with `ObligationCause::new(span)` giving
   `Misc` and a `ObligationCause::with_code(span, code)`; the codes `Misc`,
   `WhereClauseInExpr { item: GenericDefId, bound_index: u32, arg_index: Option<u32> }` (the
   callee, which of its predicates failed, and the argument it is of),
   `SizedArgumentType`, `BlockTailExpression`, `ReturnValue`, `IfExpression`,
   `MatchExpressionArm { prior_arms: u32 }`, `LetType` (a `Copy` enum of ids, so the cause stays
   small and `Copy`; rustc's boxes it). Derived obligations (`errors.rs`'s `BestObligation`,
   which makes `parent_trait_obligations`) should keep, per parent, rustc's
   `ImplDerived { impl_id: AnyImplId }` where the parent came from an impl's where clause: a
   `parent_impls: Vec<Option<AnyImplId>>` beside `FulfillmentError::parent_trait_obligations`
   would do. db-ty sets the codes at the push sites in `infer/` and carries them to
   `SolverDiagnostic`.
   > applied by db-ty-port-infer: `ObligationCause { span, code }`, `ObligationCauseCode` with
   > exactly these variants, `ObligationCause::with_code`, `.code()`; `new` gives `Misc`.
   > `FulfillmentError::parent_impls` (one per `parent_trait_obligations` entry) holds the
   > candidate impl for impl-where-bound children. `PredicateObligation` grows from 20 to at
   > most 44 bytes (asserted `<=`). Setting codes at the push sites is db-ty's.
2. `next_solver/infer/type_variable.rs` / `infer/mod.rs`: a type or const variable made by
   `var_for_def` should remember the generic parameter it was made for (rustc's
   `TypeVariableOrigin { span, param_def_id }`, `ConstVariableOrigin`): `TypeVariableSubValue`
   (or `TypeVariableData`) gains `param: Option<GenericParamId>`, `ConstVariableValue::Unknown`
   the same, set by `var_for_def`, and `pub(crate) fn type_var_origin(&self, vid: TyVid) ->
   (Span, Option<GenericParamId>)` / `const_var_origin` on `InferCtxt`. db-ty then puts the
   parameter on `TypeMustBeKnown` for rustc's ", where the type for type parameter `T` is
   specified" and the turbofish suggestion (`need_type_info.rs`, `InferSourceKind::GenericArg`).
   > applied by db-ty-port-infer: `InferCtxt::type_var_origin(vid) -> (Span, Option<GenericParamId>)`
   > and `const_var_origin(vid) -> Option<(Span, Option<GenericParamId>)>` (`None` once known,
   > as `const_var_span`). `var_for_def` records the parameter, per variable as in rustc, in
   > sparse sorted side lists (no cost for other variables), trimmed on rollback and kept
   > through `fudge_inference_if_ok`.

> forwarded by solver-sync to lane db (20:10): `infer/traits.rs`, `infer/type_variable.rs`
> and `infer/mod.rs` are db-ty-port-infer's (`v2/requests/db.md` section 10); told
> db-ty-port-infer-next2 and db-next2 by tmux.

3. (db-ty-d-infer2, follow-up to 1; for db-ty-port-infer) `next_solver/infer/traits.rs`:
   - `ObligationCauseCode` gains `Hash` in its derive. `InferenceDiagnostic::TypeMismatch` and
     `SolverDiagnostic` now carry the code and derive `Hash`. Until the derive lands,
     `solver_errors.rs` holds a hand-written `impl Hash for ObligationCauseCode`, directly
     after its imports. Delete it in the same change that adds the derive, or the two
     impls conflict.
   - Two more codes, which rustc's `CoerceMany` users set and blaze now has to call `Misc`:
     - `IfExpressionWithNoElse`: an `if` with no `else` whose `then` is not `()`
       (`check_then_else`). rustc's note: "`if` expressions without `else` evaluate to `()`".
     - `ReturnNoExpression`: a bare `return;` in a function returning something else
       (`check_expr_return`).
     Both are fieldless. db-ty then sets them in `infer/expr.rs` (the `coerce_forced_unit`
     calls of the `if` with no `else` and of `return`) and in `infer/persist.rs` (two more
     cause tags, 8 and 9).

> settled by db-solver (2026-10-29 23:45): 1 and 2 are in the tree (`ObligationCause { span, code }`, `type_var_origin`/`const_var_origin`). 3 not applied: the `Hash` derive conflicts with `solver_errors.rs:46`'s hand-written `impl Hash for ObligationCauseCode` until that is deleted in the same change, and the two new codes break the exhaustive matches in `infer/persist.rs` (~2084, ~2944) and `blaze-diagnostics/src/lower/obligation_cause.rs` (~41). None of those files is db-solver's. db-ty may make the `infer/traits.rs` half too (derive `Hash`, add `IfExpressionWithNoElse` and `ReturnNoExpression`, fieldless, after `LetType`) in the same change as its own halves (tags 9 and 10 in `persist.rs`, the impl deleted); filed in `v2/requests/db.md`.

## From lane db (db.md section 12, 20:05): CacheScope; `SimplifiedType: PartialOrd + Ord`

> applied by solver-sync (20:15): the `CacheScope` signature and items 2 and 3 of
> db-to-solver-sync.md were already carried (`vendor/rustc_type_ir` patch 0003). `SimplifiedType`
> derives `PartialOrd + Ord` as new patch 0005 (`PATCHES.md` says what and why). The series
> replays from the clean nightly to exactly the vendored tree. The items request is marked in
> `v2/requests/items.md`.

## From db-mem-infer-a-solver (2026-10-29 21:50): mem-db-solver.md sections 9 and 10 (`interner.rs`)

Lane mem's requests for `crates/hir-ty/src/next_solver/interner.rs`, copied exactly from
`v2/requests/mem-db-solver.md` sections 9 and 10 (not yet in the file as of 21:50: `Column<T>` still
segments from `alloc_zeroed`, and both walks still `let mut seen = Vec::new();` at :1905 and :2053).
The `blaze_mem::Region` API they use exists (`reserve_huge`, `base`, `committed`, `ensure` in
`crates/blaze-mem/src/region.rs`). Section 10's budget test is already written by lane db in
`next_solver/budget_tests.rs` (the block-impl variant); it passes once 10 lands.

### 9. solver-sync (`crates/hir-ty/src/next_solver/interner.rs`, `mod table`): interned storage on regions tagged `Intern`

**What is done:**
- Every interned kind is already in an append-only, never-moved, lock-free-read table of
  hir-ty's own (`interner.rs:2974-3905`): types, regions, consts, predicates, patterns,
  canonical inputs, valtrees, allocations, external constraints, clause lists, and every
  `interned_slice!` list.
- The table kinds are `Rows<K>` (columns `tag`, `flags`, `binder`, `kinds` and an `Index`),
  `Lists<T>` (columns `start`, `len`, `flags`, `binder` and a `Run` of items) and `Values<T>`
  (columns `marks`, `values`).
- A handle is a `u32` row, so the contract's shape (interned data in columns, a `u32` handle) is
  already met.

**What is left:**
- Each `Column<T>` (`:3110`) is 23 segments from `std::alloc::alloc`/`alloc_zeroed` (`:3145`).
  These are untagged, so the interner's bytes appear under no `Tag`, and a read is segment
  arithmetic (`locate`, a `leading_zeros`, two loads).
- `Run` must place a list within one segment (`fit`, `:3287`), so the end of a segment is
  skipped.
- Each `Index` shard's slot table grows by copying into a new `alloc_zeroed` table and leaking
  the old one (`Slots::allocate`, `:3353`), also untagged.

**Why not `blaze_mem::Column`:**
- Its push claims an index with its own `fetch_add` and publishes with its own ready bit.
- A `Rows` writes four columns at one row it claimed once, and publishes the row through the
  index. A `Lists` claims runs of items.
- So the tables need a reserved `Region` per column, not a `blaze_mem::Column`. The claiming and
  publishing stay the tables' own.

**What:**
- `Column<T>` becomes a lazily reserved `blaze_mem::Region` tagged `Tag::Intern`:

```rust
    /// Values by position in one reserved region, committed as positions are written and never
    /// moved or freed: a reference into it lives as long as the column (a `static`). Each
    /// position is written once, by the thread that claimed it, before anything naming it is
    /// published by a release store that the reader's acquire load pairs with.
    struct Column<T> {
        /// The region's base once reserved; null before the first write.
        base: AtomicPtr<T>,
        region: OnceLock<Region>,
        /// Positions it can hold: the reservation, as `Region::reserve` is not `const`.
        max: u64,
        of: PhantomData<T>,
    }

    impl<T> Column<T> {
        const fn new(max: u64) -> Column<T> {
            Column { base: AtomicPtr::new(ptr::null_mut()), region: OnceLock::new(), max, of: PhantomData }
        }

        #[cold]
        fn reserve(&self) -> *mut T {
            let region = self.region.get_or_init(|| {
                Region::reserve_huge(self.max as usize * size_of::<T>(), Tag::Intern)
                    .unwrap_or_else(|error| panic!("reserving an interner column: {error}"))
            });
            let base = region.base().as_ptr().cast::<T>();
            self.base.store(base, Ordering::Release);
            base
        }

        /// The region, committed through `at`, and its base.
        #[inline]
        fn reach(&self, at: u64) -> *mut T {
            let mut base = self.base.load(Ordering::Acquire);
            if base.is_null() {
                base = self.reserve();
            }
            let region = self.region.get().expect("reserved above");
            region
                .ensure((at as usize + 1) * size_of::<T>())
                .unwrap_or_else(|error| panic!("growing an interner column: {error}"));
            base
        }
    }
```

  - `write(at, value)`: `self.reach(at).wrapping_add(at as usize).write(value)`.
  - `place(at)` (zeroed columns): `&*self.reach(at).wrapping_add(at as usize)`. The kernel zeroes
    committed pages, so `Column::zeroed` and its `alloc_zeroed` go away.
  - `peek(at)`: `None` unless `base` is set and
    `(at + 1) * size_of::<T>() <= region.committed()`.
  - `get` and `slice`: `base.load(Relaxed).wrapping_add(at)`. A written position's commit and
    base store happen before the publication that the caller's acquire load pairs with.
- Each `unsafe` keeps a `// SAFETY:` comment saying what is committed and who wrote it.
- `Run::claim` drops `fit`: a run is contiguous in one region. The 2^32-position limit stays as
  `max`.
- `segment_start`, `segment_len`, `locate`, `SEGMENTS` and `FIRST_BITS` go away.
- `max` per column comes from the table: `Rows::new(what, last)` gives `last + 1` rows to its
  row columns; a `Lists` items run gives `1 << 32`; `Values` gives `LAST_ROW + 1`.
- **Address space:** reserved lazily, per column actually written.
  - The largest is `TYS`: `(2^30) × size_of::<TyKind>()`, about 32 GiB virtual.
  - A list items column is `2^32 × 8` = 32 GiB virtual.
  - With about 40 written columns, the total is under 1 TiB virtual, nothing committed until
    written. That fits a 47-bit (128 TiB) user address space.
  - Windows reserves address space without charging commit, and Linux `PROT_NONE` reservations
    are not counted by overcommit (`blaze_mem::os::reserve`).
- **The `Index` slot tables:**
  - The tables a shard outgrows can't be freed: a reader may be probing them. So all of an
    index's tables come from one region per `Index`, tagged `Intern`, bump-allocated under the
    growing shard's `len` lock, which `Vacant::insert` already holds.
  - Doubling means a shard's tables sum to under twice its last. With 64 shards at most half
    full, reserving `4 × max_rows × 8` bytes covers them with room for imbalance between
    shards.
  - `Slots::allocate` takes `&Region` and an `AtomicUsize` cursor instead of calling
    `alloc_zeroed`. Alignment is `SLOTS_ALIGN` (64) by rounding the cursor; committed memory is
    zeroed.
- **Budget and accounting tests** (`mod table` tests, beside
  `each_table_gives_the_rows_and_values_of_its_safe_reference`):
  - `interning_is_counted_under_intern`: `blaze_mem::tag::snapshot().get(Tag::Intern)` grows by
    at least the bytes of 10,000 new rows. A hit (interning the same 10,000 again) grows nothing.
  - `a_hit_allocates_nothing`: `blaze_alloc_count::assert_at_most(0, "interning a value already in", ..)`
    over interning 10,000 existing rows, lists and values. That holds now and must keep holding.
  - `a_miss_allocates_nothing`: the same over 10,000 new rows. Commits are system calls, not
    allocator calls. Today a segment's first write and a shard's growth allocate.
  - The existing safe-reference tests, the parallel test (8 threads) and the
    `a_shard_that_grows_keeps_every_row` test stay. `positions_are_in_the_segment_that_holds_them`
    and `a_run_never_crosses_a_segment` go with the segments.


### 10. solver-sync (`crates/hir-ty/src/next_solver/interner.rs`): the impl walks' `seen` lists inline

- **Why:**
  - `for_each_relevant_impl` (`:1906`) and `for_each_blanket_impl` (`:2054`) run during candidate
    assembly, once per trait goal computed.
  - Each makes `let mut seen = Vec::new();` and pushes every impl of a block (`const _` and fn
    bodies), so a trait or type defined in a block allocates per goal.
  - A block holds few impls.
- **Before:** `let mut seen = Vec::new();` (both). **After:**
  `let mut seen: SmallVec<[ImplId; 8]> = SmallVec::new();`, or `[BlanketImplId; 8]` for
  `for_each_blanket_impl`, whichever the pushed type is. hir-ty already depends on smallvec;
  `contains` is unchanged.
- **Budget test:** request 8's warm test with the fixture's `impl<T: Clone> Tr for Wrap<T>`
  moved into a `const _: () = { .. };` block.

The other 29 hot-lint findings in `next_solver/` (`opaques.rs`, `persist.rs`, `ty.rs`,
`ir_print.rs`, `util.rs`, `predicate.rs`, `valtree.rs`, and the rest of `interner.rs`) are on
paths a goal hit or a warm re-solve doesn't take. They cover:
- `const_conditions`, `opaque_types_defined_by` and `explicit_implied_const_bounds`, which are
  salsa-memoized per item;
- persistence;
- debug printing;
- reflection;
- `get_tracked`'s `.clone()` of a `Copy`-sized result.

They stay with their owners' annotations.

> forwarded to db-ty (`next_solver/interner.rs`; solver-sync is DONE; db-ty-next2, both windows); told by tmux (requests, pass 25).

> settled by db-solver (2026-10-29 23:45): see `v2/requests/mem-db-solver.md` 9 and 10.

