---
status: plan
date: 2026-10-30
summary: no-fallback in hir-ty, hir-ty-matchck and rustc_abi — each site classed as error, rule or dead, with the rustc function it matches and what it becomes.
---
# no-fallback: hir-ty, hir-ty-matchck, rustc_abi

Child `no-fallback-front-ty` of [front](../front.md). Kinds as in [no-fallback](../../no-fallback.md):
**error** (a real fallback; becomes a typed error), **rule** (rustc does the same; stays),
**dead** (deleted). "Done" in the last column means the change is in the tree.

`never!` (`stdx::never!`) panics in debug builds and only logs in release, so in a shipped
blaze each one logs and goes on. Where rustc has `bug!`/`span_bug!` at the same place, it
becomes `unreachable!`/`assert!`: the same stop rustc makes (an ICE), and no change for debug
builds or tests.

## The lines of `/tmp/fallbacks.txt`

| site | kind | what it does | rustc | becomes |
|---|---|---|---|---|
| `hir-ty/src/infer/fallback.rs` (whole file, `type_inference_fallback`, `fallback_types`, `fallback_if_possible`) | rule | unconstrained int vars become `i32`, float vars `f64` | `FnCtxt::type_inference_fallback`, `fallback_if_possible` (`rustc_hir_typeck/src/fallback.rs`) | stays |
| `hir-ty/src/infer/fallback.rs:155` `calculate_diverging_fallback` | rule | a diverging var never constrained becomes `!` | `FnCtxt::calculate_diverging_fallback` at the pinned nightly (never-type fallback to `!`) | stays |
| `hir-ty/src/infer.rs:280,283,294` | rule | fallback runs before cast checks and closure analysis | `typeck_with_inspect` order: `type_inference_fallback`, then `check_casts`, then `closure_analyze` | stays |
| `hir-ty/src/infer.rs:1106` | rule | doc of `has_errors` | — | stays (comment) |
| `hir-ty/src/infer/at_error.rs:3` | rule | doc: a literal at an error type is `i32`/`f64` after fallback | as `type_inference_fallback` | stays |
| `hir-ty/src/infer/unify.rs:296` | rule | doc: int/float vars kept for fallback | — | stays |
| `hir-ty/src/infer/unify.rs:820,824` (writeback resolver: int/float var → `i32`/`f64` after `never!`) | error | an int or float var left after fallback was logged and quietly given `i32`/`f64` | fallback resolves every one; writeback's `Resolver` never sees one | **done**: `unreachable!` |
| `hir-ty/src/infer/unify.rs` fresh ty/const vars in writeback | error | logged, made an error type, went on | `FullTypeResolver`: `bug!("Unexpected type in full type resolver")` | **done**: `unreachable!` |
| `hir-ty/src/infer/coerce.rs:237`, `coerce/fast.rs:5` | rule | coercion falls back to subtyping (`unify_and`), fast path rolls back its snapshot on failure | `Coerce::coerce` ends in `self.unify(a, b)`; `commit_if_ok` | stays |
| `hir-ty/src/infer/closure.rs:918` (`sig_of_closure_with_expectation`: merge fails → `sig_of_closure_no_expectation`, the caller at :871) | rule | a closure whose annotated signature does not merge with the expected one takes the annotated one; the mismatch is reported when the body is checked | `FnCtxt::sig_of_closure_with_expectation` (`rustc_hir_typeck/src/closure.rs`), same `Err(_) => sig_of_closure_no_expectation` | stays |
| `hir-ty/src/infer/closure.rs:1000-1017` (`supplied_sig_of_closure`: async output `next_ty_var`) | rule | the output of an async closure or block with no annotation is a fresh var; one never resolved is E0282 at writeback | `FnCtxt::supplied_sig_of_closure`, same `unwrap_or_else(...)` and `_` | stays |
| `hir-ty/src/infer/opaques.rs:91` | rule | doc: an opaque with a `HasDefiningUse` from fallback | `FnCtxt::handle_opaque_type_uses` | stays |
| `hir-ty/src/infer/pat.rs:262` | rule | edition-2024 inherited-ref matching without the feature gates is `EatBoth { consider_inherited_ref: false }` | `FnCtxt::ref_pat_matches_inherited_ref` (verbatim) | stays |
| `hir-ty/src/infer/pat.rs:1441,1461,1483` | rule | match ergonomics rule 4: fall back to the inherited reference | `FnCtxt::check_pat_ref` | stays |
| `hir-ty/src/infer/closure/analysis/expr_use_visitor.rs:562` | rule | doc: `Delegate::copy` defaults to `borrow` | `expr_use_visitor::Delegate::copy` | stays |
| `hir-ty/src/autoderef.rs:53` (`replace_infer_with_error` in the public `autoderef`) | rule | the steps given out for diagnostics' suggestions have their canonical vars as errors | not a build path: only `hir::Type::autoderef`, read by blaze-diagnostics' suggestion code (`unresolved_method_call`, `invalid_cast`, ...) | stays |
| `hir-ty/src/next_solver/generic_arg.rs:495-540` | rule | `fallback` is the callback name for a parameter with no argument or default; the caller decides (an infer var, an error arg with a diagnostic) | `GenericArgs::for_item` / `fill_rest` | stays (name only) |
| `hir-ty/src/next_solver/infer/type_variable.rs:323`, `union_find.rs:221` | rule | allocation notes for the fallback pass | — | stays |
| `hir-ty/src/next_solver/infer/errors.rs:443`, `infer/mod.rs:832` | rule | solver docs (`NormalizesTo` rigid check; `resolve_vars_if_possible`) | ported comments | stays |
| `hir-ty/src/next_solver/interner.rs:2521` | rule | a cycle's empty result | salsa cycle result, not a case | stays |
| `hir-ty/src/mir.rs:380,388,395` | rule | `SwitchTargets::otherwise`, the switch's "fallback target" | `SwitchTargets::otherwise` | stays |
| `hir-ty/src/layout.rs:147` `fallback_fmt` | rule | `LayoutCalculatorError`'s message without a context | `LayoutCalculatorError::fallback_fmt` | stays |
| `hir-ty-matchck/src/typo.rs`, `visit.rs:47,748`, `lib.rs:93,97` | rule | a catch-all binding that names a constant | `find_fallback_pattern_typo` (`rustc_mir_build/src/thir/pattern/check_match.rs`) | stays |
| `rustc_abi/src/extern_abi.rs:299` | rule | doc: `system` falls back to `C` with varargs | `ExternAbi::supports_varargs` | stays |
| `rustc_abi/src/extern_abi.rs:380` | rule | `ExternAbi::FALLBACK` | `ExternAbi::FALLBACK` | stays |
| `rustc_abi/src/layout.rs:155` | rule | `fallback_fmt` | as above | stays |

## `never!` that logs and goes on

| site | kind | what it does | rustc | becomes |
|---|---|---|---|---|
| `hir-ty/src/variance.rs:247` (FnDef, closures, coroutines) | error | logged, added no constraint | `add_constraints_from_ty`: `bug!("Unexpected unnameable type in variance computation")` | **done**: `unreachable!` |
| `hir-ty/src/variance.rs:316` (witness, placeholder, infer, binder) | error | logged, went on | `bug!("unexpected type encountered in variance inference")` | **done**: `unreachable!` |
| `hir-ty/src/variance.rs:390` (no return type) | error | logged, added no constraint for the signature | the signature always has an output | **done**: `unreachable!` |
| `hir-ty/src/variance.rs:422` (region kinds) | error | logged, went on | `add_constraints_from_region`: `bug!("unexpected region encountered in variance inference")` | **done**: `unreachable!` |
| `hir-ty/src/infer/path.rs:88` (local with no type) | error | logged, the path resolved to nothing | `FnCtxt::local_ty`: `span_bug!("no type for local variable")` | **done**: `unreachable!` |
| `hir-ty/src/infer/path.rs:426` (assoc item in a module) | error | logged, resolved to nothing | not reachable from rustc's resolver | **done**: `unreachable!` |
| `hir-ty/src/infer/expr.rs:1180` (non-ADT struct expression) | error | logged, fields left unchecked | `check_expr_struct_fields`: `span_bug!("non-ADT passed to check_expr_struct_fields")` | **done**: `assert!` it is an error type (reported where made), else panic |
| `hir-ty/src/infer/expr.rs:1289` (FRU subtyping) | error | logged, went on | `span_bug!("subtyping remaining fields of type changing FRU failed")` | **done**: `unreachable!` |
| `hir-ty/src/infer/cast.rs:372` (array ref to pointer) | error | logged, the cast went on | `check_ref_cast`: `bug!("could not cast from reference to array to pointer to array")` | **done**: `unreachable!` |
| `hir-ty/src/infer/cast.rs:665,711,715` (`pointer_kind`) | rule | logs, returns `Err`, which becomes `CastError::Unknown`, an `InvalidCast` diagnostic | `pointer_kind`: `span_delayed_bug` + `Err(guar)` (fails the build) | stays |
| `hir-ty/src/infer.rs:2901` (infer vars in an anon const's type) | error | logged, an anon const with infer vars in its type was interned | — | **done**: `assert!` |
| `hir-ty/src/infer.rs:3344,3454` (lang item path in variant position) | error | logged, error type with no diagnostic | not reachable | **done**: `unreachable!` |
| `hir-ty/src/lower.rs:3164` (assoc shorthand not found after the check) | error | logged, skipped the bound | not reachable | **done**: `unreachable!` |
| `hir-ty/src/lower/path.rs:251` (bare trait path, 0 segments left) | error | logged, error type | handled before (`lower_path`) | **done**: `unreachable!` |
| `hir-ty/src/lower.rs:1237` (pattern type of another pattern) | error | logged, error type, no diagnostic (`#![feature(pattern_types)]` only). Also `a..b` was silently lowered as `a..=b`, since `range_type` was ignored, and an open end (`1..`) was the same silent error type | `pat_to_ty_pat` ("pattern not supported in pattern types"); `lower_ty_pat_mut` completes ends with `lower_excluded_range_end` (`RangeSub::sub_one`) and `lower_ty_pat_range_end` (`RangeMin`/`RangeMax`) | **done**: `TyLoweringDiagnostic::PatternTypeNotLowered { source, reason: PatternTypeUnsupported::Pattern }` (rustc's error). hir-def already completes `a..b` and `a..` as rustc does, so those lower. Left to hir-def (queue 01283): `_` is still `Pat::Missing` there, which hir-ty can't tell from a parse error |
| `hir-ty/src/diagnostics/decl_check.rs:376,464,554,627,747` | rule | the naming-convention lint finds no AST node for a replacement and skips the lint | lint emission only; no program meaning | stays |
| `hir-ty/src/display.rs:869,916,1507,1566` | rule | rendering of a type or constant | text only | stays |

## Shapes that don't use the word

| site | kind | what it does | rustc | becomes |
|---|---|---|---|---|
| `hir-ty/src/layout.rs:351` `Normalizer::normalize`: `deeply_normalize(ty).unwrap_or(ty)` | error | a type that did not normalize was laid out as the alias | `layout_of`: `try_normalize_erasing_regions` → `LayoutError::NormalizationFailure` | **done**: `LayoutError::NormalizationFailure { ty }`, returned from `layout_of_ty` and from the pointer-metadata tail walk |
| `hir-ty/src/layout/adt.rs:117` `d.ok()?` | error | a variant whose discriminant failed to evaluate was left out of the enum's layout | `AdtDef::discriminants` + E0080 stops the build | **done**: `LayoutError::DiscriminantFailed { variant }`; test `tests/layout_errors.rs` |
| `hir-ty/src/layout/adt.rs:115` `repr_discr(..).unwrap_or((I8, false))` | error | a `#[repr]` too small for the (truncated) values became `u8` | `Integer::repr_discr`: `bug!("Integer::repr_discr: #[repr] hint too small")` | **done**: panic naming the enum |
| `hir-ty/src/infer.rs:3135` `demand_coerce` | error | a refused coercion became an error type with no diagnostic | `FnCtxt::demand_coerce` → `demand_coerce_diag`: E0308 | **done**: `emit_type_mismatch` (E0308), then the error type |
| `hir-ty-matchck/src/cx.rs:206` field types: `deeply_normalize(ty).unwrap_or(ty)` | error | a field type that did not normalize was checked as the alias | `ctor_sub_tys`: `normalize_erasing_regions` (panics) | **done**: panic naming the field; bodies with errors are skipped before (`lib.rs:291`) |
| `hir-ty/src/lower.rs:936,944` `lower_expr_as_const` / `lower_path_as_const` `unwrap_or(consts.error)` | error | a const argument in a type that does not lower (unresolved, a static or local, a forbidden generic, a literal of the wrong type) is an error const with no diagnostic | E0425 (the pinned nightly's, not E0412) / E0435 / E0013 / "generic parameters may not be used in const operations" / E0308 | **done**: `TyLoweringDiagnostic::InvalidConstArg { site, reason: CreateConstError }` (lowering words tag 3), a `_` where no inference is E0121 (`InferVarsNotAllowed`), and a literal of another type is an anon const whose own type check gives E0308, as rustc's; test `tests/const_args.rs`. Emitter: queue 01188 |
| `hir-ty/src/infer.rs:2928,2935` `create_anon_const` / `make_path_as_const` `unwrap_or_else(next_const_var(Span::Dummy))` | error | in a body the same failure becomes a **fresh inference var**, which other constraints can solve: a program rustc rejects can type-check. `Span::Dummy` also hides it from E0282 | as above; only `_` (`UnderscoreExpr`) is a var in rustc | **done**: `InferenceDiagnostic::InvalidConstArg { site, reason }` (persist tag 46) and an error const; a literal of another type is E0308 (`emit_type_mismatch`); `TypeLikeConst::Infer` gets its type ref as span; `CreateConstError` lost its lifetime, splits `ResolveToNonConst` into `LocalBinding`/`Static`/`ImplSelf`, gains `TargetLayoutNotAvailable`; integer literals truncate as rustc's `lit_to_const`. hir: `AnyDiagnostic::InvalidConstArg`. Emitter: queue 01188 |
| `hir-ty/src/infer/unify.rs` `err_on_span` returns on a dummy span; const var with no span | error | an unresolved var made with `Span::Dummy` becomes an error type with no E0282 | `Resolver::report_error` always reports | after the const-arg fix removes the `Span::Dummy` vars, report or `unreachable!` |
| `hir-ty/src/mir.rs:2142` place projection: `structurally_normalize` `Err(_) => error` | error | a place type that does not normalize becomes an error type | `PlaceTy::projection_ty`: `normalize_erasing_regions` (panics) | fixed by no-fallback-front: panics naming the type and the solver's errors, as rustc's `normalize_erasing_regions` does (the body type-checked, so its place types normalize) |
| `hir-ty/src/mir.rs:2177,2183` Deref/Index of a non-pointer: error type | error | "lowering a deref pattern on a smart pointer gets here": an unsupported feature becomes an error type | `bug!("deref projection of non-dereferenceable ty")`; deref patterns are lowered differently | fixed by no-fallback-front: panics naming the type, as rustc's `bug!`; deref patterns on smart pointers call `Deref::deref` first (`hir-ty-mir` `lower/deref_pattern.rs`), so only a MIR-building bug reaches it |
| `hir-ty/src/layout.rs:593,650,654,711,716` `LayoutError::Unknown` | error (untyped) | a MIR or const failure under a layout becomes `Unknown` | rustc's `LayoutError::ReferencesError` / `TooGeneric` / `Unknown(ty)` | stays an error; typed variants later (not a fallback) |
| `hir-ty/src/consteval/hir_eval.rs` `by_hir` → `by_mir` | rule | constants of a closed shape run over HIR, all others over MIR; both give the same bytes and errors (`docs/v2/plan/lanes/frontend/fast-const.md`) | — (blaze speed path) | stays: dispatch by shape, not on failure. `ok()?` at :147,:439 hide an error the MIR path then reports |
| `hir-ty/src/consteval.rs:73,81,88` `bit_width().unwrap_or(pointer_size)` | rule | `usize`/`isize` have no fixed width | `Integer::from_int_ty` | stays |
| `hir-ty/src/consteval.rs:75,83,90` literal not fitting → `Const::error` | error | a const-generic literal too large for its type is an error const; rustc truncates it (and `overflowing_literals` denies by default) | `lit_to_const`: `trunc` | queued with the const-arg diagnostics |
| `hir-ty/src/layout/adt.rs:84,89,96`, `layout.rs:271,429`, `consteval/discriminants.rs:42` `AttrFlags::repr(..).unwrap_or_default()` | rule | no `#[repr]` is the default repr | `AdtDef::repr` default | stays |
| `hir-ty/src/traits.rs:82` `structurally_normalize_ty` | rule | "used in `hir` only" (IDE) | — | stays |
| `hir-ty/src/inhabitedness.rs:133` | rule | an alias that does not normalize is opaquely inhabited | `InhabitedPredicate::GenericType` is inhabited when unknown | stays |
| `hir-ty/src/method_resolution/confirm.rs:506` | rule | receiver mismatch reported only with `arbitrary_self_types` | `ConfirmContext::unify_receivers` (else `span_delayed_bug`; wfcheck reports E0307) | stays |
| `hir-ty/src/infer/pat.rs:1318`, `infer/expr.rs:160`, `infer/pat.rs:1760`, `infer/cast.rs:246`, `infer/coerce.rs:910,1026`, `infer/expr.rs:2014,2102` | rule | error recovery after a diagnostic is pushed | the same `FnCtxt` functions | stays |
| `rustc_abi/src/*` (`unwrap_or` in `lib.rs`, `layout.rs`; `layout.rs:628` `.ok()?` in niche filling) | rule | copies of rustc's `rustc_abi` (`LayoutCalculator::layout_of_enum` tries a niche layout and takes the tagged one when it fails) | `rustc_abi` verbatim | stays |
| `hir-ty-matchck/src/lower.rs:968-1000`, `print.rs:37,39`, `visit.rs:809` | rule | open range ends; float text; printing; a witness's type | `rustc_pattern_analysis` / `check_match` | stays |

## Silent error types (`FIXME: report`)

Each of these gives `TyKind::Error` or an error const/region **without a diagnostic**, and
no other pass reports the code (checked in blaze-diagnostics and hir: none emits E0220,
E0223, E0225, E0393, E0562, E0719, E0071). A program rustc rejects can get through, or fail
later at a layout with no place. Each becomes a typed diagnostic variant (the rustc code
with its place). That means a new `TyLoweringDiagnostic`/`PathLoweringDiagnostic`/
`InferenceDiagnostic` variant, its words in `lower/persist.rs` or `infer/persist.rs`, an arm in
`hir`'s exhaustive `path_diagnostic` / inference match, and an emitter in blaze-diagnostics.
All of them have to land in one save to compile, across three owners. They are queued as one
request (see the report).

| site | rustc |
|---|---|
| `lower/path.rs:243` assoc type not found on a trait path | E0220 `complain_about_assoc_item_not_found`. **Done**: `PathLoweringDiagnostic::AssocTypeNotResolved { qself: Trait }` |
| `lower/path.rs:179,254` ambiguous associated type (`Trait::Assoc`, more than one segment) | E0223 `report_ambiguous_assoc_item`. **Done**: `PathLoweringDiagnostic::AmbiguousAssocType`, also for `<u8>::X` and an inherent one not found; not of a self type that is an error |
| `lower/path.rs:534,555` `Self::Name` / `T::Name` with no such assoc type | E0220, E0221 ambiguous, E0391 cycle. **Done**: `AssocTypeNotResolved { qself: Param, failure }` |
| `lower/path.rs:274` an enum variant or module as a type | rule: E0573 "expected type, found variant" is the resolver's, which blaze-diagnostics `checks/names/paths.rs` reports from names (tested in `tests/items/names.rs`); lowering gives the error type after it |
| `lower/path.rs:1386` `<T as Struct>::X` | rule: E0404 "expected trait, found .." is the resolver's, which `checks/names/paths.rs` reports for the trait of `<T as Trait>` |
| `lower.rs:960,974,996` a forbidden generic parameter (a default naming a later one) | E0128 (blaze-diagnostics `checks/defs/generics.rs` reports it from names; to confirm it covers every case here) |
| `lower.rs:1188` `impl Trait` where not allowed | E0562. **Done**: `TyLoweringDiagnostic::ImplTraitNotAllowed` (the emitter reads the position from the syntax, and says blaze does not support it under `impl_trait_in_bindings`/TAIT) |
| `lower.rs:1649` a second non-auto trait in `dyn` | E0225. **Done**: `AdditionalNonAutoTraits { first, additional }` |
| `lower.rs:1660` a second lifetime bound in `dyn` | rule: E0226, which blaze-diagnostics `ast_validation/types.rs` reports |
| `lower.rs:1710` conflicting assoc bindings (no error type: the later value was kept silently) | E0719. **Done**: `ConflictingAssocTypeBindings { source, assoc: AssocItemId }` |
| `lower.rs:1829` `Self` in a `dyn` default | E0393. **Done**: `MissingSelfDefaultParams { source, trait_, params }`; projections that name `Self` stay rustc's `delayed_bug` (another error is reported) |
| `lower.rs:1885` `dyn` with no principal resolved | E0224. **Done** for bounds that are all relaxed (`dyn ?Sized`): `ObjectWithoutTrait`; no trait written is the parser's E0224, an unresolved path is reported where it is resolved |
| `infer.rs` `resolve_variant`, `resolve_variant_on_alias`: struct path that resolves to a non-struct, or with extra segments | E0574 / E0532 for an item of another kind, E0071 for a type, E0599 no such variant, E0223, E0433 after a variant. **Done**: `InferenceDiagnostic::NotAStructPath { node, found: StructPathFound, ty }`. Two cases went unreported before and were wrongly accepted: `Self::A::B { .. }` and `Trait::A::B { .. }` gave `A`'s struct. Then done too: in a tuple struct pattern, a last segment after a type is resolved as rustc's `resolve_fully_qualified_call` resolves it. An associated function or constant there is `StructPathFound::TypeRelative` (E0164), and a missing one is the probe's E0599. `Self::new(x)` was lowered as an associated type (E0223) and `Tr::f(x)` as E0220; `Tr::f(x)` is now E0532 "found associated function". Generic arguments on a variant that a struct path reaches through a type (`<E>::V::<T>`, `Alias::V::<T>`, `Self::V::<T>`) are E0109 (`GenericArgsProhibitedReason::TypeRelativeVariant`). Then done too: `T::Assoc { .. }`, `T::A::V { .. }` and `T::A::V(..)` on a type parameter resolve through its bounds and normalize, as rustc's `check_struct_path` does. They used to be an error type with no diagnostic. One that does not normalize to a struct is E0071, and `T::A::f(x)` in a tuple struct pattern is E0164. Arguments on a type-relative variant in a value path or tuple struct pattern (`Alias::V::<T>(x)`) are E0109 too: rustc's `instantiate_value_path` sets `is_alias_variant_ctor` and prohibits them. Before, blaze ignored them without a word (a generic enum) or checked them against the variant (a non-generic one). A trait item in a tuple struct pattern that is neither a function nor a constant is still reported as E0220. Then done too: `E::A::X { .. }`, which hir-def's resolver resolves to nothing (no segment after a variant), was an error type with no diagnostic; it is E0433 `AfterVariant` |
| `infer.rs` `resolve_variant` `TypeNs::AdtSelfType` (`Self { .. }` inside an anon const of the ADT's own definition, e.g. an array length) | error, **open**: an error type with no diagnostic. rustc's resolver gives `SelfTyAlias` (a generic ADT's is "generic `Self` types are currently not permitted in anonymous constants"), then type-checks it against the ADT. To do: resolve it as the `SelfType` arm does and pin rustc's error for the generic case |
| `lower.rs` `anonymous_lifetime` under `LifetimeElisionKind::AnonymousCreateParameter` (an elided lifetime in an impl header, `impl Send for &T`) | error region, **open**: rustc's resolver makes it a fresh early-bound parameter of the impl (`fresh_lifetime`). blaze gives `'{region error}`, which unifies with anything, so the solver still matches it, but the shape table's `impl_args` can't. That is the suspect for round 7c's `global_goals` `a_shape_hit_allocates_nothing` and `a_region_only_auto_trait_bound_turns_the_shape_table_off`, both failing on `List: Send` through minicore's `Send for &T`. It is also queue 01225's cause (round 10, `tests::impossible_items`): `for<'a> &'a Item: Mark` fails through `impl<T: ?Sized> Mark for &T` but holds through `&'static`, so rustdoc leaves out a method whose bound only holds through core's `impl Copy for &T`. To do: give the impl those parameters in `generics` and lower them as `ReEarlyParam` |
| `dyn_compatibility.rs` `receiver_is_dispatchable` with no `Unsize`, `DispatchFromDyn` or `MetaSized` lang item | quiet `false`, **open**: every `&self` method becomes `UndispatchableReceiver`, so the trait is dyn-incompatible and no call on a `dyn` of it resolves. rustc's `require_lang_item` stops with an error naming the item. Only a fixture without them reaches this (queue 01277: blaze-lints' minicore had no `dispatch_from_dyn`). To do: a typed error naming the missing lang item, sent with its blaze-diagnostics side |
| `infer/path.rs:98,189` invalid `Self` reference, first segment unresolved | **Done** (98): a `Self` with no constructor (see `infer/pat.rs:1053`). Rule (189): `infer_path`'s callers report the unresolved segment (`UnresolvedIdent` E0425, `UnresolvedPath` E0433) |
| `infer/closure.rs:512` closure signature deduction obligations dropped | rule: rustc drops them too (`deduce_sig_from_projection` "may discard the goal") |
| `method_resolution.rs:219` a private method picked | rule: E0624 is reported by blaze-diagnostics `lower/private_assoc_item.rs` from the pick |
| `method_resolution/probe.rs:411` autoderef reached the recursion limit while probing | E0055 `report_autoderef_recursion_limit_error`. **Done**: `InferenceDiagnostic::AutoderefRecursionLimit`, reported by every inference walk (the table's, `place_op`, `callee`, coercion) as rustc's `Autoderef::next` does, and by the probe from its steps. The walk's limit was a hard-coded 20, so a receiver more than 20 derefs deep got no method; it is now the crate's `recursion_limit` (128 by default) |
| `method_resolution/probe.rs:425` method on a raw pointer to an unknown pointee, edition 2018+ | E0282 with rustc's label (E0699 is no longer emitted). **Done**: the edition test was backwards (2018+ went on silently, 2015 errored); now `MethodOnUnknownRawPointee` from 2018. Then done too: in 2015, the warn-by-default lint `tyvar_behind_raw_pointer` (`InferenceDiagnostic::TyvarBehindRawPointer`), once per call |
| `method_resolution/probe.rs:451` receiver that derefs to a type variable | E0282 from `probe_op`. **Done**: `TypeMustBeKnown` at the call, unless the body is already tainted. Before, a variable resolved later in the body left the failed call unreported |
| `method_resolution/confirm.rs:619` | rule: the allow-by-default `supertrait_item_shadowing_usage` lint; no program meaning |
| `infer/pat.rs:594` deref pattern past the recursion limit | E0055. **Done**: `AutoderefRecursionLimit` at the pattern |
| `infer/pat.rs:1053` a path pattern naming a function, static, local or generic | E0532 / E0530 / E0158. **Done**: `NotAStructPath` with `StructPathFound::Def`. Then done too: a function reached through a type is E0533 (`TypeRelative`), not E0532. A `Self` path pattern that is not a unit struct is E0533. A `Self` expression with no constructor is `SelfCtorNotTupleOrUnit`; it was an E0425 for a record struct and nothing for an enum. A unit struct's `Self` pattern, which rustc accepts, used to fail with no diagnostic and no variant resolution |
| `infer/pat.rs:1426,1464,1501` `&mut` pattern on an inherited shared ref (`ref_pat_eat_one_layer_2024*` only) | E0308. **Done**: `InheritedRefMutabilityMismatch` |
| `infer/expr.rs:1259,1351` missing fields | rule: E0063 is reported by blaze-diagnostics `lower/missing_fields.rs` |
| `infer/expr.rs:2319` `Fn*` call whose `Args` is not a tuple (`unboxed_closures` only) | E0059. **Done**: `FnTraitArgsNotTuple` |
