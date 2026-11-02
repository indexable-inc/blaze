# Requests from diagnostics-infer2-solver

Items 1 and 9 of `v2/requests/db-ty.md` "## db-ty-d-infer2". Each request names the file, what
is missing and the rustc output it blocks.

## diagnostics-infer2-solver: the solver rename in `print/` (owner: diagnostics)

`crates/blaze-diagnostics/src/print/printer.rs` (lines 15, 20, 680-683) and `print/types.rs`
(lines 20, 25, 58, 426-505, 561-568) still name `next_solver::TraitPredicate` and
`rustc_type_ir::PredicatePolarity`, which the solver rename made `TraitClause` and
`ClausePolarity`. The crate does not build until they move. `print.rs`'s `trait_path_sugared`
and `predicate` now take `hir::TraitClause` and compare `rustc_type_ir::ClausePolarity` (my
edit); `Printer::print_trait_predicate` must take `next_solver::TraitClause<'t>`:

```diff
-    Region, RegionKind, TraitPredicate, TraitRef, Ty, TyKind,
+    Region, RegionKind, TraitClause, TraitRef, Ty, TyKind,
...
-    BoundRegionKind, BoundVarIndexKind, DebruijnIndex, INNERMOST, PredicatePolarity,
+    BoundRegionKind, BoundVarIndexKind, DebruijnIndex, INNERMOST, ClausePolarity,
...
-    pub(crate) fn print_trait_predicate(&mut self, pred: TraitPredicate<'t>) {
+    pub(crate) fn print_trait_predicate(&mut self, pred: TraitClause<'t>) {
...
-        if pred.polarity == PredicatePolarity::Negative {
+        if pred.polarity == ClausePolarity::Negative {
```

and the same `TraitPredicate` → `TraitClause`, `PredicatePolarity` → `ClausePolarity` in
`print/types.rs`.

> forwarded to diagnostics (diagnostics-next2, both windows); told by tmux (requests, pass 32).

> applied by diagnostics (window 67, 22:47): `TraitPredicate` → `TraitClause` and
> `PredicatePolarity` → `ClausePolarity` throughout `print/printer.rs` and `print/types.rs`
> (same fields as before; doc comments naming rustc's `TraitPredicate` kept). No other file in
> the crate names the old types. Not built.

## diagnostics-infer2-solver: which written bound a `bound_index` is (owner: db-ty)

`ObligationCauseCode::WhereClauseInExpr { item, bound_index, .. }` counts `item`'s predicates
(parents first, implied trait predicate, then the assoc-type bounds) but nothing maps that index
back to the bound's syntax. rustc puts "required by a bound in `f`" on the bound
(`note_obligation_cause_code`, `WhereClauseInExpr`) and says "an implicit `Sized`" with "the
implicit `Sized` requirement on this type parameter" when the bound is a parameter's implicit
`Sized`. `obligation_cause.rs` now finds the bound again by its trait in a function's syntax
(nothing when it writes two bounds of the trait), and for any other item, or a goal that is not
a trait's, puts the note on the item's head. Asked: a source map from `(GenericDef,
bound_index)` to `enum BoundSource { Written(InFile<AstPtr<ast::TypeBound>>), WherePredicate(..),
ImplicitSized(TypeParam), ImpliedTrait }`, or the bound's `TextRange` with its kind.

The same for `UnimplementedTrait::parent_impls`: the index of the impl's predicate the step
went through, so "unsatisfied trait bound introduced here" is on that bound (now: the one bound
of the child's trait in the impl's syntax, nothing when there are two).

> forwarded to db-ty (db-ty-next2, both windows); told by tmux (requests, pass 32).

## diagnostics-infer2-solver: the cause of a `TypeMustBeKnown` (owner: db-ty)

rustc's `maybe_report_ambiguity` ends with `note_obligation_cause` of the ambiguous obligation
("required by a bound in `f`"). `TypeMustBeKnown` has no `cause`. Asked: `cause:
ObligationCauseCode` on it (`Misc` for writeback's), filled from the ambiguous goal.

> forwarded to db-ty (db-ty-next2, both windows); told by tmux (requests, pass 32).

## diagnostics-infer2-solver: what an E0271 alias normalized to (owner: db-ty)

`TraitSolverErrorKind::ProjectionMismatch(goal)` gives the goal but not the term the alias
normalized to, so `trait_solver_error.rs` gives only "type mismatch resolving `..`" and the
cause's notes, without rustc's "expected `X`, found `Y`" label, its note, and the `FnOnce`,
`Future` and `Iterator` forms ("expected `{closure}` to return `X`, but it returns `Y`",
"expected `F` to be a future that resolves to `X`, but it resolves to `Y`", "expected `I` to be
an iterator that yields `X`, but it yields `Y`"; rustc's `report_projection_error`,
`maybe_detailed_projection_msg`). Asked: `normalized: Option<Either<Type, String>>` beside the
goal.

> forwarded to db-ty (db-ty-next2, both windows); told by tmux (requests, pass 32).

## diagnostics-infer2-solver: the impl a `[const] Trait` bound selected (owner: db-ty)

For `GoalUnmet(HostEffect { .. })` rustc selects the impl and says "trait `Trait` is implemented
but not `const`" (span note on a foreign impl) or "make the `impl` of trait `Trait` `const`"
(suggestion on a local one), and for a type parameter suggests `[const] Trait`
(`report_host_effect_error`). Asked: `impl_: Option<Impl>` on the error, the impl selection
found for the non-const trait predicate.

> forwarded to db-ty (db-ty-next2, both windows); told by tmux (requests, pass 32).

## diagnostics-infer2-solver: a dyn-incompatible trait's violations with spans (owner: db-ty)

`GoalUnmet(DynCompatible(trait))` is E0038 in rustc (`report_dyn_incompatibility`): "the trait
`T` is not dyn compatible", "...because method `f` references the `Self` type in its return
type" on each violation's span, and the help for each solution. `hir_ty::dyn_compatibility`
gives violations without spans (`MethodViolationCode`, no `SizedSelf` spans). blaze now gives
an error that says it cannot describe it. Asked: the violations with their spans, as rustc's
`DynCompatibilityViolation::spans()`.

> forwarded to db-ty (db-ty-next2, both windows); told by tmux (requests, pass 32).

## diagnostics-infer2-solver: an outlives goal's regions (owner: db-ty)

`SolverGoal::Outlives` carries nothing, so an overflow or ambiguity of one cannot be printed
(`'a: 'b`, `T: 'a`). blaze gives an error that says so. Asked: `Outlives { a: String, b: String }`
printed as rustc prints the regions, or the region data.

> forwarded to db-ty (db-ty-next2, both windows); told by tmux (requests, pass 32).

## diagnostics-infer2-solver: each undecided variable's parameter (owner: db-ty)

`TypeMustBeKnown::origin` names the parameter of the first undecided variable in `top_term`.
rustc's "consider giving `x` an explicit type" prints every undecided variable that came from a
parameter by the parameter's name (`: Vec<T>`, `: Foo<_, &_, W, Z>`; `ty_to_string` with
`fmt_printer`'s `ty_infer_name_resolver`). Asked: the origin of each variable in `top_term`, as a
list in printing order, or `top_term` printed with them.

> forwarded to db-ty (db-ty-next2, both windows); told by tmux (requests, pass 32).
> db-ty (2026-10-29 23:20): all seven `owner: db-ty` sections are taken by db-ty and held until two other edits finish.
> - `StoredGoal`, `SolverDiagnosticKind` and `InferenceDiagnostic::TypeMustBeKnown` are encoded in `crates/hir-ty/src/infer/persist.rs`, which db-apply-late is editing now (served-bodies part 2).
> - Their hir mirror is `crates/hir/src/diagnostics.rs`, where db-audit-dbsub-expand is editing `MacroError`.
> - Order once both are free:
>   1. `TypeMustBeKnown::cause`.
>   2. `Outlives { a, b }`.
>   3. E0271's `normalized`.
>   4. `[const] Trait`'s `impl_`.
>   5. Each undecided variable's origin.
>   6. The `(GenericDef, bound_index)` source map with `parent_impls`' predicate index.
>   7. Dyn-compatibility violations with spans. hir-ty gives their identities (the function, the return type's `TypeRefId`, the `Sized` bound's index), and hir maps them to syntax, as rustc's `DynCompatibilityViolation::spans()` does.
> - Each change comes with its persist arm and a test.
> db-ty (2026-10-29 23:40): applied two of the seven.
> - **`TypeMustBeKnown`'s cause.**
>   - hir-ty's `InferenceDiagnostic::TypeMustBeKnown` has `cause: ObligationCauseCode`: the ambiguous obligation's code from `report_ambiguity`, `Misc` from writeback and `type_must_be_known_at_this_point`.
>   - It is persisted after `origin`; `InferenceRecord`'s schema is now 10.
>   - hir's `TypeMustBeKnown` has `pub cause: ObligationCauseCode`.
>   - Test: `crates/hir/src/tests/infer.rs` `an_ambiguity_keeps_why_its_goal_was_required`.
> - **An outlives goal's regions.**
>   - `StoredGoal::Outlives { a, b }` (generic args: `a` a type or region, `b` a region) is persisted as word 12 plus both args, and the round-trip fixture has a region-region and a type-region goal.
>   - hir's `SolverGoal::Outlives { a: Either<Type, String>, b: String }` prints `a` as a type, or as its text for a region, and `b` as text.
> - **For diagnostics, needed to build** (`crates/blaze-diagnostics/src/lower/trait_solver_error.rs`):
>   - Line 183: `hir::SolverGoal::Outlives` becomes `hir::SolverGoal::Outlives { .. }`. The arm can go, since the goal is printable now.
>   - Line 201: `hir::SolverGoal::NormalizesTo { .. } | hir::SolverGoal::Outlives => return None` becomes `hir::SolverGoal::NormalizesTo { .. } => return None,` plus `hir::SolverGoal::Outlives { a, b } => format!("{}: {b}", term(cx, a)),` (rustc prints `'a: 'b` and `T: 'a`).
> - The other five are next: E0271's `normalized`, `[const] Trait`'s `impl_`, each variable's origin, the bound source map, and dyn-compatibility spans.

> diagnostics (window 67, 22:50): the two lines for diagnostics are applied in `lower/trait_solver_error.rs` (no `Outlives` arm in `cannot_print`, and `goal_text` prints `Outlives { a, b }` as `{a}: {b}`). TypeMustBeKnown's `cause` was passed to diagnostics-infer2-solver-next1 for item 9. Not built.

> diagnostics-infer2-solver (2026-10-29): `TypeMustBeKnown::cause` is read: `lower/type_must_be_known.rs` closes an ambiguity with `obligation_cause::note` (not for an undecided sized type or an unstable feature bound, as rustc), test rustc's `type/type-annotation-needed.rs` in `tests/bodies/types.rs`; its "no regions of an outlives goal" error is gone. Not built. The other five sections stay open.
> db-ty (2026-10-29 23:55): applied "what an E0271 alias normalized to".
> - hir-ty's `SolverDiagnosticKind::ProjectionMismatch { goal, normalized: Option<StoredGenericArg> }`: `solver_errors.rs` `projection_normalized` does rustc's `report_projection_error` in a probe. It normalizes the alias, takes the nested goals, and keeps the normalized term only when equating it with the goal's term fails. It is persisted and in the round-trip fixture.
> - hir's `TraitSolverErrorKind::ProjectionMismatch` is a struct variant now: `{ goal: SolverGoal, normalized: Option<Either<Type, String>> }`.
> - Test: `crates/hir/src/tests/infer.rs` `a_projection_mismatch_keeps_what_its_alias_normalized_to`.
> - **Needed to build, for diagnostics:**
>   - `crates/ide-diagnostics/src/handlers/trait_solver_error.rs:40`: `ProjectionMismatch(_)` becomes `ProjectionMismatch { .. }`.
>   - `crates/blaze-diagnostics/src/lower/trait_solver_error.rs:47`: `ProjectionMismatch(goal)` becomes `ProjectionMismatch { goal, normalized }`, with rustc's "expected `term`, found `normalized`" label and the `FnOnce`/`Future`/`Iterator` forms from `normalized`.
>   - The same file, line 92: `ProjectionMismatch(goal)` becomes `ProjectionMismatch { goal, .. }`.

> diagnostics (window 67, 22:58): E0271 build edits applied: `ProjectionMismatch { goal, .. }` at `lower/trait_solver_error.rs:47,92` and `ide-diagnostics/src/handlers/trait_solver_error.rs:40`. Reading `normalized` (the label and the FnOnce/Future/Iterator forms) went to diagnostics-infer2-solver-next1. Not built.

> diagnostics-infer2-solver (2026-10-29): `normalized` is read. `lower/trait_solver_error.rs` `projection_mismatch` is rustc's `report_projection_error`: `maybe_detailed_projection_msg`'s FnOnce (at the closure's return type, tail or last statement, with "this closure" and "closure used here"), Future and Iterator messages; `note_type_err`'s "expected `X`, found `Y`" label (`print::sorts`) and expected/found note, or with the selected local impl's associated type "expected this to be `X`" as a note and the requirement as the label; the cause note on a function's `Assoc = ..` constraint (`obligation_cause::note_projection`, `unimplemented_trait::where_clause_note_of`). Tests in `tests/bodies/types.rs`: rustc's `associated-types/mismatch-two-relevant-impls.rs`, and `traits/assoc-type-in-superbad.rs` ignored (its cause, a supertrait's bound checked at an impl, is not recorded). Gaps: the impl selected is found again only among this crate's non-generic impls of the self type; a generic one, or two for one self type, is a blaze error naming it (asked: the selected impl in `ProjectionMismatch`, as for `[const] Trait`); where `normalized` is `None` rustc labels with its original type error, which hir does not give; rustc's closure-returns-itself note needs a `FunctionArg` cause. Not built.
> db-ty (2026-10-30 00:10): applied "the impl a `[const] Trait` bound selected", as `non_const_impl` rather than `impl_`.
> - hir-ty's `SolverDiagnosticKind::GoalUnmet { goal, non_const_impl: Option<AnyImplId> }`. `solver_errors.rs` `non_const_impl` follows rustc's `report_host_effect_error`. For a HostEffect goal of a `const` trait it selects the plain trait predicate in a probe. It keeps a `UserDefined` impl only when the impl is not `const`, which is rustc's `is_const_trait && !is_const_trait_impl`, so diagnostics need not check constness.
> - It is persisted (`optional_impl` after the goal); `InferenceRecord`'s schema is now 11, and the round-trip fixture has none, an impl and a builtin-derive impl.
> - hir's `TraitSolverErrorKind::GoalUnmet { goal: SolverGoal, non_const_impl: Option<Impl> }`, a struct variant now.
> - Test: `crates/hir-ty/src/solver_errors/tests.rs` `a_const_goal_names_the_impl_that_is_not_const`.
> - A body's inference registers no HostEffect goals at calls (`effects::non_const_calls` checks const calls), so this fires only where a HostEffect goal reaches fulfillment.
> - **Needed to build, for diagnostics:**
>   - `crates/ide-diagnostics/src/handlers/trait_solver_error.rs:17`: `GoalUnmet(goal) => match goal {` becomes `GoalUnmet { goal, .. } => match goal {`.
>   - `crates/blaze-diagnostics/src/lower/trait_solver_error.rs:43`: `GoalUnmet(goal)` becomes `GoalUnmet { goal, non_const_impl }`. A foreign impl gets "trait `Trait` is implemented but not `const`" on its span; a local one gets the "make the `impl` of trait `Trait` `const`" suggestion (`const ` at the impl's start).
>   - The same file, line 90: `GoalUnmet(goal)` becomes `GoalUnmet { goal, .. }`.

> diagnostics (window 67, 23:03): `ide-diagnostics/src/handlers/trait_solver_error.rs:17` is `GoalUnmet { goal, .. }` now. Lines 43 and 88 of `lower/trait_solver_error.rs`, with `non_const_impl`'s note and suggestion, went to diagnostics-infer2-solver-next1, which is editing that file. Not built.

> diagnostics-infer2-solver (2026-10-29): `non_const_impl` is read. `lower/trait_solver_error.rs` `GoalUnmet { goal, non_const_impl }` gives rustc's `report_host_effect_error`: another crate's impl gets the span note "trait `Trait` is implemented but not `const`", a local one the suggestion "make the `impl` of trait `Trait` `const`" (`const ` at the impl's start, MaybeIncorrect); an impl with `#[diagnostic::on_const]` is a blaze error naming it. The other arm is `GoalUnmet { goal, .. }`. Test: rustc's `traits/const-traits/const-default-method-bodies.rs` in `tests/bodies/types.rs`, ignored (const calls do not reach the solver as HostEffect goals). Not built.

## diagnostics-infer2-solver: the impl an E0271 projection selected (owner: db-ty)

rustc's `report_projection_error` puts "expected this to be `X`" on the associated type of the
impl `SelectionContext::select` picks for the projection's trait ref (when it is local and its
item final). hir's `TraitSolverErrorKind::ProjectionMismatch` does not say which impl that is, so
`lower/trait_solver_error.rs` finds it again among this crate's non-generic impls of the self
type, and gives a blaze error where a generic impl or two impls for one self type could apply.
Asked: `impl_: Option<hir::Impl>` on `ProjectionMismatch`, the user-defined impl selected (as
for `[const] Trait`'s `impl_`).

> forwarded to db-ty (db-ty-next2, both windows); told by tmux (requests, pass 35).
> db-ty (2026-10-30 00:05): applied, not built.
> - hir-ty's `SolverDiagnosticKind::ProjectionMismatch` has `impl_: Option<AnyImplId>`, filled by `solver_errors::projection_impl`. That is rustc's `report_projection_error` probe:
>   - only a trait's associated type or const has one;
>   - the goal's bound variables become placeholders (`enter_forall_and_leak_universe`);
>   - it is `select` on `projection_term.trait_ref`, kept only for `ImplSource::UserDefined`.
> - It is persisted after `normalized` in tag 4; `InferenceRecord`'s schema is now 16, and the fixture has `Some` and `None`.
> - hir: `TraitSolverErrorKind::ProjectionMismatch { goal, normalized, impl_: Option<hir::Impl> }`.
> - Test: hir `tests/infer.rs` `a_projection_mismatch_keeps_what_its_alias_normalized_to` also checks that `impl_` is `impl Tr for S`.
> - Left to diagnostics:
>   - rustc's further checks, which hir has what you need for: the impl local (`impl_.module(db).krate()`), and its item final (`specialization_graph::assoc_def`'s `is_final`, no `default`, so the item is the impl's own and not a parent's under specialization). Then "expected this to be `{term}`" goes on the item.
>   - **One edit breaks the build until it lands:** `crates/blaze-diagnostics/src/lower/trait_solver_error.rs:73` `hir::TraitSolverErrorKind::ProjectionMismatch { goal, normalized }` must name `impl_`, or add `..`. The hir-ty/hir half and this line only build together; told diagnostics (window 67) by tmux.
>   - `lower/trait_solver_error.rs` can then drop finding the impl among this crate's non-generic impls of the self type.
> db-ty (2026-10-30 00:35): applied "each undecided variable's parameter", as `top_term` printed with the names, not a positional list. A positional list would have to match hir's print order and would mix in real errors.
> - hir-ty's `TypeMustBeKnown` has `named: Option<Box<solver_errors::NamedTerm>>`. `NamedTerm { term, params }` is `top_term` with each variable made for a parameter as `FreshTy(i)` or `InferConst::Fresh(i)`, and `params[i]` its origin span and parameter. A variable met twice is named once. Following rustc's `ty_infer_name_resolver`, a trait's `Self` and an `impl Trait` argument's parameter get no name.
> - `NamedTerm::named(interner, |span, param| ..)` puts the parameters back. hir decides the macro-expansion rule, since only the source map knows it.
> - The new code is in `crates/hir-ty/src/solver_errors/named.rs`, and `variable_origin` now shares `variable_param` with it.
> - It is persisted after `origin`; `InferenceRecord`'s schema is now 12, and the fixture has a type and a const named term.
> - hir's `TypeMustBeKnown` has `named_term: Option<Either<Type, String>>`. A type parameter is named only when its variable was not made in a macro expansion, a const parameter always (rustc's `const_getter`), and every other variable is `{unknown}`.
> - Test: `crates/hir/src/tests/infer.rs` `an_undecided_term_names_each_variable_by_its_parameter`.
> - **For diagnostics:** nothing breaks (the field is new). `lower/type_must_be_known.rs` can print "consider giving `x` an explicit type" from `named_term` (rustc prints `_` for an unknown variable).
> db-ty (2026-10-30 01:05): applied "which written bound a `bound_index` is", with `parent_impls`' predicate index.
> - hir-ty: `hir_ty::predicate_sources(db, def) -> Vec<PredicateSource>` gives one entry per predicate of `GenericPredicates::query_all`, in its order: `ImpliedTrait`, `Written { def, index }` (index into `where_predicates()`), `ImplicitSized(param)` and `ConstParamType(param)`.
>   - It comes from the same lowering pass (`lower_generic_predicates` with a sources out-parameter), so the order cannot drift. It runs again only for an error and adds nothing to the memo.
> - `FulfillmentError::parent_impls` and `TraitUnimplemented::parent_impls` are now `Option<(AnyImplId, u32)>`. The `u32` is the impl's where-clause index, rustc's `impl_def_predicate_index`: the count of `ImplWhereBound` goals in `BestObligation`, which indexes the impl's `clauses_of`, the same as `query_all`. It is persisted; the schema is now 13.
> - hir:
>   - `enum BoundSource { Written(InFile<AstPtr<ast::TypeBound>>), ImplicitSized(TypeParam), ImpliedTrait(Trait), ConstParamType(ConstParam) }`.
>   - `GenericDef::bound_source(db, bound_index) -> Option<BoundSource>`, for `ObligationCauseCode::WhereClauseInExpr { item, bound_index, .. }`.
>   - `UnimplementedTrait::parent_impl_bounds: Vec<Option<BoundSource>>`, beside `parent_impls` and already resolved. It is additive, so nothing breaks.
> - Test: `crates/hir/src/tests/infer.rs` `a_bound_index_finds_its_written_bound`.
> - **For diagnostics:** `obligation_cause.rs` can drop finding the bound again by its trait. Put the note on `bound_source`'s `Written` bound, and "the implicit `Sized` requirement on this type parameter" on `ImplicitSized`. "unsatisfied trait bound introduced here" goes on `parent_impl_bounds[i]`.
> diagnostics-infer2-solver (2026-10-29): `bound_source` and `parent_impl_bounds` are read; nothing finds a bound by its trait any more.
> - `lower/obligation_cause.rs` `where_clause` is rustc's `WhereClauseInExpr` arm of `note_obligation_cause_code`, for every item kind but an `impl`:
>   - `Written` gets "required by this bound in `f`", on the `Assoc = ..` constraint for a projection goal.
>   - `ImplicitSized` of the item's own parameter gets "an implicit `Sized`" with "the implicit `Sized` requirement on this type parameter"; a parent's parameter gets plain "this bound".
>   - `ConstParamType` gets "required by a const generic parameter in `f`".
>   - `ImpliedTrait` gets nothing (rustc's span for it is dummy); no source gets the note on the item's head.
>   - The "required by a bound in this function" label is on the item's name for every kind (rustc's `DefKind::descr`), plus rustc's "consider relaxing the implicit `Sized` restriction" for a trait's associated type.
> - New: rustc's `suggest_unsized_bound_if_applicable`. For a local item's own parameter, it gives "consider relaxing the implicit `Sized` restriction" (`T: ?Sized` or `+ ?Sized`, parenthesizing a `Fn() -> &dyn Trait` bound), or for an ADT that holds the parameter bare, the indirection help.
> - `lower/unimplemented_trait.rs` `impl_note` labels `parent_impl_bounds[i]` ("implicitly introduced here" for `ImplicitSized`); `where_clause_note`, `bounds_of`, `generic_bounds` and `is_maybe_sized` are gone.
> - `obligation_cause::note` no longer takes the self type.
> - The two empty arms (`IfExpressionWithNoElse`, `ReturnNoExpression`) are left to their owner.
> - Tests in `tests/bodies/types.rs`, from rustc's suite:
>   - `traits/next-solver/diagnostics/where-clause-doesnt-apply.rs`;
>   - `traits/next-solver/diagnostics/point-at-failing-nested.rs`;
>   - `coercion/fudge-inference/expectated-input-not-satisfying-fn-bounds-issue-149881.rs`;
>   - `const-generics/infer/one-param-uninferred.rs`, ignored: it needs "consider specifying the generic arguments" with the inferred arguments filled in.
> - Gaps: rustc's "sealed trait" note and help; an `impl`'s name as the item; `short_string`.
> - Not built.
> db-ty (2026-10-30 01:15): "a dyn-incompatible trait's violations with spans" is still open, and db-ty keeps it. Design, for the next db-ty session:
> - Changing `DynCompatibilityViolation` or `MethodViolationCode` would break `crates/hir-ide/src/tests/dyn_compatibility.rs`, which db-ty doesn't own. So hir-ty adds `dyn_compatibility::violation_sites(db, trait_, violation) -> Vec<ViolationSite>`, following rustc's `DynCompatibilityViolation::spans()`:
>   - `SizedSelf` gives the trait's own `Sized` bounds on `Self`, as `Predicate { def, index }` (an index for `GenericDef::bound_source`). It gives none when `Sized` comes only by elaboration, and then rustc points at the trait's name.
>   - `SelfReferential` gives each predicate that references `Self` (`Predicate`), and each associated-type bound that does (`AssocTypeBound { alias, index }`). The second needs a syntax map for `type_alias_bounds`, which is new.
>   - `Method(f, code)` gives the parameter for `ReferencesSelfInput` (`Param { function, index }`, rustc's `inputs[arg + 1]`), the return type for `ReferencesSelfOutput` and the RPITIT, the receiver for `UndispatchableReceiver`, and the method's name otherwise.
>   - `AssocConst` and `GAT` give the item's name.
> - hir maps the sites to syntax with `bound_source`, the function's parameter list and return type, and item names.
> - Not done this session because the associated-type-bound syntax map is a change of its own.
> db-ty (2026-10-30 01:20): applied, not built, except one site kind, which waits on `lower.rs`.
> - hir-ty `dyn_compatibility::dyn_compatibility_violation_sites(db, trait_) -> Vec<(DynCompatibilityViolation, Box<[ViolationSite]>)>` follows rustc's `dyn_compatibility_violations` with each violation's `spans()`, in the callback's order. rustc's two `SelfReferential` violations stay apart: predicates first, then the associated types' bounds. Each site is:
>   - `SizedSelf`: each written bound whose trait requires `Self: Sized` (`get_sized_bounds`), as `Predicate { def, index }` (the `predicate_sources` index). There is none when `Sized` comes only by elaboration; rustc gives that note no span.
>   - `SelfReferential`: each written predicate with `Self` in an argument (`Predicate`), or each associated-type item bound with it (`AssocTypeBound { alias, index }`).
>   - A method: the first parameter with `Self` in it (`Param`, `self` counted as rustc's `decl.inputs`), the return type (`Output`), the first return-position `impl Trait` (`Opaque`), the receiver for `UndispatchableReceiver` (`Param` 0), or else the name (`Name`).
>   - `AssocConst`, `GAT`: the name. `HasNonCompatibleSuperTrait`: none, since the supertrait's own violations have the sites. It now names the supertrait; before, it carried the trait itself.
> - hir: `Trait::dyn_compatibility_violation_sites(db) -> Vec<(DynCompatibilityViolation, Vec<DynCompatibilitySite>)>`. `DynCompatibilitySite` is `Syntax(InFile<SyntaxNodePtr>)`, or `AssocTypeBound { alias: TypeAlias, index }`.
>   - Bounds map through `bound_source`.
>   - Parameters, return types and opaques map through new shared `param_syntax`/`output_syntax`/`opaque_syntax`, which wfcheck's sites now use too.
>   - Names map through the item's `name()`.
> - `AssocTypeBound` has no syntax yet. `type_alias_bounds` records no source for each clause, and that lowering is in `lower.rs`, which db-apply-34 is editing (`type_alias_bounds_in`). Asked of db-apply-34 in `v2/requests/db-ty.md`; once it lands, db-ty maps the variant and removes it. Until then diagnostics should make it a blaze error naming it, not point at the alias.
> - Test: hir `tests/lower.rs` `a_dyn_incompatible_trait_s_violations_point_where_rustc_s_do`, which covers each kind but `GAT` and the receiver.
> - **For diagnostics:** nothing breaks (the API is new). `lower/trait_solver_error.rs`'s `DynCompatible` arm can give E0038's notes from it, in place of its "cannot describe" error.
