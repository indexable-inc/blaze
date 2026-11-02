# Requests from db-matchck

## 1. Root `Cargo.toml`: the `hir-ty-matchck` workspace dependency (db-matchck)

What: `hir-ty-matchck = { path = "crates/hir-ty-matchck", version = "0.0.0" }` in
`[workspace.dependencies]`, beside `hir-ty-borrowck`.
Where: the root `Cargo.toml`.
Why: already asked by lane db in `v2/requests/db.md` §9; repeated here so it is not missed.
`crates/hir/Cargo.toml` and the crate's own tests name it.

> applied once the crate has its `Cargo.toml`: the manifest lint refuses a workspace path with none, and `crates/hir-ty-matchck/` has none yet (db.md 9 waits the same way).
> applied: the root `Cargo.toml` has it (line 89), and `crates/hir/Cargo.toml` takes it.

## 2. Root `Cargo.toml`: `rustc_apfloat` as a workspace dependency (db-matchck)

What: `rustc_apfloat = "0.2.3"` in `[workspace.dependencies]` (the version `crates/hir-ty-mir`
already pins directly; switch `crates/hir-ty-mir/Cargo.toml` to `rustc_apfloat.workspace = true`
in the same change).
Where: the root `Cargo.toml`.
Why: float literal and range patterns are compared and printed through IEEE values, as rustc
does (`F32Range` / `F64Range` constructors of `rustc_pattern_analysis` take `rustc_apfloat`
values' bits; range validity uses `partial_cmp`).

> applied: `rustc_apfloat = "0.2.3"` is in the root `[workspace.dependencies]` already (line 174). `crates/hir-ty-mir/Cargo.toml` is codegen's: forwarded to codegen (told by tmux).
> applied: root `rustc_apfloat = "0.2.3"` beside the other rustc crates; `crates/hir-ide/Cargo.toml` takes it with `workspace = true`. > forwarded to codegen: the `crates/hir-ty-mir/Cargo.toml` switch (blaze-diagnostics, hir-ty, hir-def and syntax may switch too).


## dedup: stdx::edit_distance

From `dedup-scan` (lane dedup; report `/tmp/blaze-lanes/v2-dedup-scan.md`). `crates/hir-ty-matchck/src/distance.rs`
is one of six copies of rustc's `edit_distance` and three of `find_best_match_for_name`; the one
copy is `stdx::edit_distance` (`v2/requests/coordinator.md`, `## dedup: one stdx::edit_distance`;
this file's two tests are in its tests, `distances_are_rustcs` verbatim and
`best_match_prefers_case_then_distance_then_words` over a name function). `stdx` is already a
dependency. rustc's `find_best_match_for_name` with `dist: None` is what this file computes
(the default third-of-length limit, no substring score). Apply once that lands:

- Delete `src/distance.rs` and `mod distance;` in `src/lib.rs` (line 17).
- `src/typo.rs` line 15: `use crate::{PatternTypo, cx::Cx, distance::find_best_match_for_name};`
  becomes `use crate::{PatternTypo, cx::Cx};` and `use stdx::edit_distance::find_best_match_for_name_by;`
- `src/typo.rs` line 46: `find_best_match_for_name(&found.accessible, name)` becomes
  `find_best_match_for_name_by(&found.accessible, Name::as_str, name.as_str(), None)`;
  line 56: `find_best_match_for_name(&found.imported, name)` becomes
  `find_best_match_for_name_by(&found.imported, Name::as_str, name.as_str(), None)`.
  (Both return `Option<&Name>` as before; `Name` is already imported there.)

Status: accepted, waiting (db-matchck, 2026-10-29 20:39). `stdx` has no `edit_distance`
module yet and `v2/requests/coordinator.md` has no `## dedup: one stdx::edit_distance` section,
so `distance.rs` stays until `stdx::edit_distance::find_best_match_for_name_by` exists. Then the
three edits above are applied as written; the call shape matches `typo.rs` (both calls take
`&[Name]` and return `Option<&Name>`).

> forwarded to db (from requests, pass 16; told by tmux).
> not applied by db-audit-dbsub (2026-10-29 22:35): waits on the coordinator's `v2/requests/coordinator.md` "## dedup: one `stdx::edit_distance`" (filed, on requests' coordinator list); `crates/stdx/src/` has no `edit_distance.rs` yet, so `distance.rs` stays until it lands, then the three edits above as written.
> applied by apply-00215 (2026-10-30, queue 00215): the three edits above as written (`typo.rs`'s import is now on line 15), `src/distance.rs` deleted.

## 3. diagnostics (blaze-diagnostics, ide-diagnostics): the new match checking fields (db-matchck)

What: `hir`'s match checking types now come from `hir-ty-matchck` (rustc's `check_match`).
The callers that read the old `uncovered_patterns: String` or match exhaustively over these
enums need to move to these types:

- `hir::MissingMatchArms<'db>` (E0004): `scrutinee_expr: InFile<AstPtr<ast::Expr>>`,
  `scrutinee_ty: Type<'db>`, `no_arms: bool` (rustc's "type `T` is non-empty" for a type that is
  not an enum with variants), `witnesses: Vec<String>` (every witness as `print_witness_pat`
  prints it; rustc shows the first 3), `variants: Vec<EnumVariant>`
  (`maybe_point_at_variant` of the first 5), `special_tys: Vec<(Type<'db>, SpecialTy)>`
  (`collect_special_tys` of the first witness; only for a match with arms),
  `would_be_exhaustive_without_guards: bool`, `ref_to_uninhabited: bool` ("references are always
  considered inhabited"), `consts_like_bindings: Vec<(InFile<AstPtr<ast::Pat>>, Const)>` (arms
  whose pattern is a constant that looks like a binding, in arm order).
- `hir::NonExhaustiveLet<'db>` (E0005): `pat: InFile<ExprOrPatPtr>` (an expression for a
  destructuring assignment), `origin: BindingOrigin { Let, FnParam, ClosureParam, ForLoop,
  Assignment }` (`Assignment` prints as "local binding", as rustc lowers it to a `let`),
  `has_initializer: bool`, `pattern_ty: Type<'db>`, `witnesses`, `variants` (only for an ADT of
  this crate, `point_at_non_local` false), `special_tys` (always empty for E0005, as rustc),
  `privately_uninhabited: bool`, `interpreted_as_const: Option<Const>`.
- `hir::SpecialTy { Usize, Isize, Str, ForeignNonExhaustive, Uninhabited }`: which note rustc's
  `report_non_exhaustive_match` adds for the type.
- New `AnyDiagnostic` variants: `EmptyRangePattern { pat, inclusive }` (E0030 for `..=`, E0579
  for `..`), `PatternComplexityLimit { at: InFile<SyntaxNodePtr> }` ("reached pattern complexity
  limit"), `MixedDerefPatterns { deref_pat, normal_pat }` ("mix of deref patterns and normal
  constructors").
- `hir::MatchLint` gains `OverlappingRangeEndpoints { pat, overlap: String, overlaps_with }` and
  `NonContiguousRangeEndpoints { pat, gap: String, suggestion: String, gapped_with }`
  (`gapped_with` empty is rustc's `ExclusiveRangeMissingMax`; `suggestion` is the `lo..=hi` text).
  `hir::LetSource` stays `{ IfLet, IfLetGuard, WhileLet, LetElse }` (`else if let` folds into
  `IfLet`, as rustc words both alike).
- `DefWithBody::match_check_failed(db)`: rustc's `check_match` returned `Err`, or the body was
  not checked because type checking failed. In both cases rustc builds no MIR of the body, so the
  body gets no MIR lint (`mir_tainted`).

Where: `crates/blaze-diagnostics/src/lower/{mod.rs (the three new AnyDiagnostic arms),
missing_match_arms.rs:34, non_exhaustive_let.rs}`, `crates/blaze-diagnostics/src/lints/bodies/patterns.rs:35`
(the two range lints), and while it exists `crates/ide-diagnostics/src/lib.rs:531-535` and
`handlers/{missing_match_arms,non_exhaustive_let}.rs` (`d.uncovered_patterns` becomes
`d.witnesses.join(", ")`; the exhaustive `AnyDiagnostic` match needs the three new arms).
Why: the old string came from hir-ide's `match_check.rs`, which made every literal, range and
constant `_`. These are rustc's own facts, so the lowering no longer re-parses witness text.

> told by tmux (window `diagnostics`, 2026-10-29).
> forwarded to diagnostics (from requests, pass 16; told by tmux).

## 4. db (hir-ide): drop hir-ide's match checking (db-matchck)

What: delete `validate_match` / `check_non_exhaustive_let` and `BodyValidationDiagnostic::{MissingMatchArms,
NonExhaustiveLet}` from `crates/hir-ide/src/diagnostics/expr.rs`, with
`diagnostics/match_check.rs` and `match_check/pat_analysis.rs` if nothing else reads them.
`crates/hir/src/diagnostics.rs:2083` ignores those two variants now.
Where: `crates/hir-ide/src/diagnostics/`.
Why: no-fallback: two match checkers must not both run, and hir-ide's would still cost a pass per
body. `hir-ty-matchck` replaces it.

> taken by requests (hir-ide is requests'): child `requests-db-fixture`, which holds hir-ide now; the outcome follows. hir's `diagnostics.rs:2115-2117` arms go in the same compile (db's file, requested of db).
> applied by requests (child requests-db-fixture): `validate_match`, `is_known_valid_scrutinee`, `check_non_exhaustive_let`, `lower_pattern`, `types_of_subpatterns_do_match`, `missing_match_arms` and the two variants are gone from `diagnostics/expr.rs` (`validate_block` keeps only the unused-must-use check); `diagnostics/match_check.rs` and `match_check/{pat_analysis,pat_util}.rs` deleted, with `mod match_check;`, `extern crate rustc_pattern_analysis` and hir-ide's `typed-arena`/`ra-ap-rustc_pattern_analysis` dependencies. hir's arm (`hir/src/diagnostics.rs` ~L2280-2283) is requested in `v2/requests/requests.md` for the same compile; db-next2 told. Nothing built.

## 5. db-ty (hir-ty): inhabitedness ignoring visibility (db-matchck, optional)

What: `hir_ty::inhabitedness`: a mode of `UninhabitedFrom` that ignores field visibility (rustc's
`inhabited_predicate(..).apply_ignore_module`).
Where: `crates/hir-ty/src/inhabitedness.rs`.
Why: `hir-ty-matchck/src/visit.rs` (`uninhabited_ignoring_visibility`) has its own walk of it,
for `witness_1_is_privately_uninhabited`. One rule in one place; once it lands, the matchck copy
goes.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-ty-next3 (2026-10-29 ~22:35): `hir_ty::inhabitedness::is_enum_variant_privately_uninhabited(infcx,
> variant, args, local: Crate, env)`, the same walk as `is_enum_variant_uninhabited_from` with
> every field visible. The walk now also has rustc's `variant_list_has_applicable_non_exhaustive`
> (`Ty::inhabited_predicate`): a `#[non_exhaustive]` enum of another crate is inhabited, in both
> modes. Test: `crates/hir-ty/src/tests/inhabitedness.rs`. For db-matchck, in
> `hir-ty-matchck/src/visit.rs` `privately_uninhabited`: replace
> `variant_uninhabited_ignoring_visibility(cx, variant, args, 0)` with
> `is_enum_variant_privately_uninhabited(cx.infcx, variant, args, cx.krate, cx.env)`, then delete
> `variant_uninhabited_ignoring_visibility`, `uninhabited_ignoring_visibility` and
> `MAX_TYPE_DEPTH`. That changes one thing: the copy took a `#[non_exhaustive]` struct or
> variant of another crate as inhabited, which rustc does not (`VariantDef::inhabited_predicate`
> reads only the fields; rustc `compiler/rustc_middle/src/ty/inhabitedness/mod.rs`), and it
> normalizes aliases now, as rustc's `apply_ignore_module` does.

## 6. Owner of the hir items (db-matchck)

Settled (lane db confirmed, 2026-10-29). db-matchck-hir owns `crates/hir/src/match_lints.rs`,
the match-check items in `crates/hir/src/diagnostics.rs`, the `crates/hir` `lib.rs` and
`Cargo.toml` lines, and `crates/hir-ty-matchck/src/tests.rs` with `src/tests/`. db-matchck owns
the rest of `crates/hir-ty-matchck/` and sends changes for those files to db-matchck-hir by tmux
rather than editing them. db-matchck's earlier edits there, made while db-matchck-hir's window was
closed, are now db-matchck-hir's: hir's four-variant `LetSource`, `ConstPatternError` (with its
`lib.rs` `pub use`), `ConstPatternRefused` in `diagnostics.rs`, and `// alloc:` notes.

> from db-matchck-hir (tmux window 103, a second agent continuing db-matchck): that agent wrote
> `crates/hir/src/match_lints.rs`, the five diagnostics types and the call in
> `crates/hir/src/diagnostics.rs`, the `crates/hir` manifest line and
> `crates/hir-ty-matchck/src/tests/ui.rs` (more rustc UI cases, through `tests.rs`'s `check`), by
> the split the user chose (2026-10-29). Lane db confirmed the split: db-matchck-hir owns
> `crates/hir/src/match_lints.rs`, the `diagnostics.rs` items above, the `lib.rs` and
> `Cargo.toml` lines, and `crates/hir-ty-matchck/src/tests.rs` with `src/tests/` (report
> `/tmp/blaze-lanes/v2-db-matchck-hir.md`). Briefed by tmux: window 62 and db-ty-next2 (window 89).
> forwarded to db (from requests, pass 16; told by tmux).
> checked by db-audit-dbsub (2026-10-29 22:35): a settled ownership note, no edit asked; the files it names are in the tree (`crates/hir/src/match_lints.rs`, `crates/hir-ty-matchck/src/tests.rs`, `src/tests/`).

## 7. db (hir-ty-borrowck, hir): no borrow checking of a body match checking refused (db-matchck)

What: skip `hir_ty_borrowck::borrowck` (or make it return nothing) for a body where
`hir_ty_matchck::match_check(db, def)` has `failed` or `not_checked`; in `crates/hir/src/diagnostics.rs`
that is the `collect_borrowck(def, body, source_map)` call right after
`match_lints::match_check_diagnostics`.
Where: `crates/hir-ty-borrowck/src/lib.rs` (`borrowck`), or `collect_borrowck` in
`crates/hir/src/diagnostics.rs`.
Why: rustc's `mir_built` gives the error body when `check_match` returns `Err`, so a body with
E0004, E0005 or E0030 gets no borrow checking error. Blaze would report errors rustc does not.
`crates/blaze-diagnostics/src/lints/bodies.rs:132` already stops the MIR lints this way
(`match_check_failed`).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-ty-next3 (2026-10-29 ~22:20) in `hir_ty_borrowck::borrowck` itself, so every
> caller (hir's diagnostics, doctests, the panics lint) agrees: `check_def` returns
> `NotChecked::MatchErrors` when `hir_ty_matchck::match_check(db, def).failed` (after the type and
> lowering error checks; `not_checked` is already `NotChecked::TypeErrors` there). New dependency
> `hir-ty-matchck` in `crates/hir-ty-borrowck/Cargo.toml`. Test:
> `crates/hir/src/tests.rs` `a_body_match_checking_refused_gets_no_borrow_check_error`.

## 8. frontend (hir-def): `never_patterns` in `UnstableFeatures` (db-matchck)

What: a `never_patterns: bool` field in `UnstableFeatures`, set by `#![feature(never_patterns)]`.
Where: `crates/hir-def/src/unstable_features.rs`.
Why: under `never_patterns` rustc prints an uninhabited witness as `!` rather than `_`
(`print_witness_pat`); `crates/hir-ty-matchck/src/print.rs:103` always prints `_`. A crate
with the feature then gets different E0004/E0005 text.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-audit-dbsub (2026-10-29 22:45), not built: `never_patterns` in `crates/hir-def/src/unstable_features.rs`; its symbol `sym::never_patterns` (intern, filed in `v2/requests/db.md`) is in at `crates/intern/src/symbol/symbols.rs:692`; db-matchck-next2 (asked by tmux) wired it into `hir-ty-matchck`: `Cx::never_patterns` (`cx.rs:110`, from `visit.rs:98`), and `print.rs` prints `Constructor::Never` as `!` and not as a wildcard under the feature (`:93-103`, `:266`), as rustc's `print_witness_pat`.
> applied: `never_patterns` was already in `UnstableFeatures`. db-matchck (2026-10-29 ~22:40)
> reads it: `Cx::never_patterns`, set in `visit.rs`; `print.rs` prints `Constructor::Never` as
> `!` and doesn't count it as a wildcard under it, as rustc's `print_witness_pat`. Not built.

## 9. diagnostics (blaze-diagnostics): lower the refused constant patterns (db-matchck)

What: `AnyDiagnostic::ConstPatternRefused(hir::ConstPatternRefused { pat: InFile<AstPtr<ast::Pat>>,
error: hir::ConstPatternError<'db> })` is rustc's `const_to_pat` refusal of a constant used as a
pattern, at the path pattern. Each part of the value `valtree_to_pat` refuses gets its own
diagnostic. Lower it the way rustc's `rustc_mir_build/src/errors.rs` does, and give it an arm in
the exhaustive `AnyDiagnostic` matches (`blaze-diagnostics/src/lower/mod.rs`, and
`ide-diagnostics/src/lib.rs` while that crate exists):
- `Union`: "cannot use unions in constant patterns";
- `Pointer`: "function pointers and raw pointers not derived from integers in patterns behave
  unpredictably";
- `Invalid(ty)`: "{prefix} `{ty}` cannot be used in patterns", with `Ty::prefix_string`;
- `NotStructural(ty)`: `TypeNotStructural`, "constant of non-structural type `{ty}` in a
  pattern", with its notes and the `SuggestEq` suggestions;
- `NotPartialEq(ty)`: `TypeNotPartialEq`, "constant of type `{ty}` cannot be used in patterns"
  with "the type must implement `PartialEq`";
- `NaN`: "cannot use NaN in patterns".
Where: `crates/blaze-diagnostics/src/lower/`.
Why: rustc refuses these programs; today blaze accepts them.

> forwarded to diagnostics (from requests, pass 17; told by tmux).

## 10. test-utils (minicore): `Option` derives `PartialEq` (db-matchck)

What: `#[derive(PartialEq, Eq)]` (behind `// region:eq` with `derive`) on minicore's `Option`,
as core has it.
Where: `crates/test-utils/src/minicore.rs:1609`.
Why: a constant of an ADT type is a pattern only when the ADT derives `PartialEq` (rustc's
`StructuralPartialEq`). Without the derive, `hir-ty-matchck`'s `match_arm_statics` test (a
`const NONE: Option<Foo>` arm) gets `NotStructural`, where rustc's `match-arm-statics.rs` builds.

> forwarded to the coordinator (`crates/test-utils/`) (from requests, pass 17; on requests' coordinator list).

## 11. hir-expand: an expanded `#[derive(PartialEq)]` implements `StructuralPartialEq` (db-matchck)

What: when builtin derives are expanded (`set_expand_builtin_derives(db, true)`), have
`partial_eq_derive` also emit `impl<..> ::core::marker::StructuralPartialEq for T<..> {}`, with
no bounds on the parameters, as rustc's `expand_deriving_partial_eq` does (`skip_path_as_bound`).
Where: `crates/hir-expand/src/builtin/derive_macro.rs:1394`.
Why: `hir-ty-matchck/src/lower.rs` (`is_structural`) finds a recorded builtin derive or a
`StructuralPartialEq` impl. In the expanded mode there is neither, so a derived constant there
would be refused.

> forwarded to db (db-expand-macros-next1 holds `crates/hir-expand/`) (from requests, pass 17; told by tmux).
> applied by db-expand: by db-expand-macros (2026-10-29, not built). `partial_eq_derive` writes `impl<..> $crate::marker::StructuralPartialEq for T<..> where <the item's own bounds> {}` first, as rustc's `expand_deriving_partial_eq` does. It bounds no parameter by the trait (`skip_path_as_bound`) or by `Copy`, is written for a union too beside `PartialEq`'s error (`supports_unions: true`), and stays non-`const` under `derive_const` (`is_const: false`). `reference.rs` follows. Test: `partial_eq_writes_structural_partial_eq_first` (`derive_macro/tests.rs`), and `builtin_derive_macro.rs`'s expectations. Needs minicore's `marker::StructuralPartialEq` for fixtures (requested of the coordinator in `v2/requests/db-expand.md`).

## 12. hir (`match_lints.rs`, db-matchck-hir): carry the `PartialEq` facts of a refused constant (db-matchck)

What: `hir_ty_matchck::ConstPatternError` changed (2026-10-29 22:45, for diagnostics' request
12 in `v2/requests/diagnostics.md`): `NotStructural(ty)` is now
`NotStructural { ty, inapplicable_structural: bool }`, and `NotPartialEq(ty)` is now
`NotPartialEq { ty, local_manual: Box<[ManualPartialEq]>, local_without: Box<[AdtId]>, manual:
Box<[StoredTy]>, without: Box<[StoredTy]> }`, with the new
`pub enum ManualPartialEq { Impl(ImplId), Adt(AdtId) }` (docs on each in `lib.rs`). Until
hir follows, `crates/hir/src/match_lints.rs` doesn't compile (the two arms at lines 359-364).
Where: `crates/hir/src/match_lints.rs` and the `match_lints` `pub use` in `crates/hir/src/lib.rs`.
Suggested edit:

```rust
pub enum ConstPatternError<'db> {
    // ...
    /// The value holds this ADT, which doesn't derive `PartialEq` (`TypeNotStructural`).
    /// `inapplicable_structural`: it has a `StructuralPartialEq` impl whose bounds don't hold,
    /// so rustc says nothing of a manual `PartialEq` impl.
    NotStructural { ty: Type<'db>, inapplicable_structural: bool },
    /// `ty` doesn't implement `PartialEq` (`TypeNotPartialEq`), with what rustc's
    /// `extend_type_not_partial_eq` finds in it (see `hir_ty_matchck::ConstPatternError`).
    NotPartialEq {
        ty: Type<'db>,
        local_manual: Box<[ManualPartialEq]>,
        local_without: Box<[Adt]>,
        manual: Box<[Type<'db>]>,
        without: Box<[Type<'db>]>,
    },
    // ...
}

/// Where the note about a local manual `PartialEq` impl points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManualPartialEq {
    Impl(Impl),
    Adt(Adt),
}

// in the `MatchError::ConstPattern` arm:
matchck::ConstPatternError::NotStructural { ty: it, inapplicable_structural } => {
    ConstPatternError::NotStructural {
        ty: ty(it),
        inapplicable_structural: *inapplicable_structural,
    }
}
matchck::ConstPatternError::NotPartialEq { ty: it, local_manual, local_without, manual, without } => {
    ConstPatternError::NotPartialEq {
        ty: ty(it),
        local_manual: local_manual
            .iter()
            .map(|&it| match it {
                matchck::ManualPartialEq::Impl(id) => ManualPartialEq::Impl(id.into()),
                matchck::ManualPartialEq::Adt(id) => ManualPartialEq::Adt(id.into()),
            })
            .collect(),
        local_without: local_without.iter().map(|&id| Adt::from(id)).collect(),
        manual: manual.iter().map(|it| ty(it)).collect(),
        without: without.iter().map(|it| ty(it)).collect(),
    }
}
```

Then `blaze-diagnostics/src/lower/const_pattern_refused.rs` (diagnostics) reads them.
Why: rustc's `TypeNotPartialEq` notes and the `NotStructural`→`NotPartialEq` rule (diagnostics'
request 12). Told by tmux.

> forwarded to db-matchck (db-matchck-next2); told by tmux (requests, pass 33).

> applied by db-matchck-next2 (2026-10-29 ~22:58), as written above, not built: db-matchck-hir
> has no window and the tree didn't compile without it (requests pass 33 gave it to either).
> Also `ManualPartialEq` is in hir's `match_lints` `pub use` in `crates/hir/src/lib.rs`. The
> next break is `blaze-diagnostics/src/lower/const_pattern_refused.rs:44-45` (diagnostics), told
> by tmux with the two-line fix.
> checked by db-audit-other (23:23): in the tree at `crates/hir/src/match_lints.rs:71-100` and `:382-405`, matching `hir-ty-matchck/src/lib.rs:249-285` (`Impl: From<ImplId>` at `from_id.rs:257`, `Adt: From<AdtId>` at :57). `const_pattern_refused.rs:44-45` is still the old form, and diagnostics-next2 was reminded with the fix.

> applied by diagnostics (diagnostics-next2, 00:20). `const_pattern_refused.rs` reads `NotStructural { ty, inapplicable_structural }` and `NotPartialEq { ty, local_manual, local_without, manual, without }`. NotStructural follows rustc's `(possibly_inapplicable_structural_partial_eq, non_blanket_impl)` match: with an impl that doesn't apply there is neither note, the suggestion needs a local ADT, and `SuggestEq` reads "if `T` manually implemented `PartialEq`, you could ...". NotPartialEq is `extend_type_not_partial_eq`: span notes at `local_manual`, labels at `local_without`, then the `manual` and `without` notes sorted by text. Tests: rustc's `derive-and-manual-partialeq-issue-147714.rs` and `reject_non_partial_eq.rs` with their `.stderr`, in `tests/bodies/const_patterns.rs`. Not built.
