//! What type inference records for the diagnostics that read it: the assignment an invalid
//! left-hand side is of, scope errors of jumps and `yield`, where a body stops being reachable,
//! an operator's unmet predicates and a call's arguments, the goals left ambiguous, the value
//! each path resolved to, the first segment of a path that names nothing, and what a
//! `Self: Sized` method called on a trait object picked.

use hir_def::{
    AdtId, DefWithBodyId, GenericDefId, GenericParamId, ItemContainerId, Lookup,
    expr_store::Body,
    hir::{ArithOp, BinaryOp, Expr, ExprId, ExprOrPatId, generics::GenericParams},
    resolver::ValueNs,
    signatures::{FunctionSignature, StructSignature, TraitSignature},
};
use hir_ty::{
    CastError, EditionLint, InferBodyId, InferenceDiagnostic, InferenceResult, Unreachable,
    UnreachableAt,
    method_resolution::CandidateId,
    next_solver::{TyKind, default_types, infer::traits::ObligationCauseCode},
    solver_errors::{SolverDiagnostic, SolverDiagnosticKind, StoredGoal, TypeAnnotationNeeded},
};
use rustc_type_ir::{
    InferTy, TypeVisitableExt as _,
    inherent::{GenericArgs as _, IntoKind as _},
};
use hir_expand::fixture;

use super::{db::TestDB, function};

/// The body of the function `name` and what inference found in it.
fn infer<'db>(db: &'db TestDB, name: &str) -> (&'db Body, &'db InferenceResult<'db>) {
    let def = function(db, name);
    (Body::of(db, def), InferenceResult::of(db, InferBodyId::DefWithBodyId(def)))
}

/// The first expression of `body` that `pred` holds for.
fn find_expr(body: &Body, pred: impl Fn(&Expr<'_>) -> bool) -> ExprId {
    body.exprs().find(|(_, it)| pred(it)).map(|(id, _)| id).expect("the expression is in the body")
}

const ASSIGN: &str = r#"
//- minicore: sized, add
fn plain() {
    1 = 2;
}
fn compound() {
    let x = 0;
    1 += x;
}
fn place() {
    let mut x = 0;
    x = 1;
    x += 1;
}
"#;

#[test]
fn an_invalid_left_hand_side_names_its_assignment() {
    let db = fixture::db(ASSIGN).into_db();
    crate::attach_db(&db, || {
        let (body, result) = infer(&db, "plain");
        let assign = find_expr(&body, |it| matches!(it, Expr::Assignment { .. }));
        let found: Vec<_> = result
            .diagnostics()
            .iter()
            .filter_map(|it| match *it {
                InferenceDiagnostic::InvalidLhsOfAssignment { assign, op, .. } => Some((assign, op)),
                _ => None,
            })
            .collect();
        assert_eq!(found, [(assign, None)], "`=` is E0070");

        let (body, result) = infer(&db, "compound");
        let assign = find_expr(&body, |it| {
            matches!(it, Expr::BinaryOp { op: Some(BinaryOp::Assignment { op: Some(_) }), .. })
        });
        let found: Vec<_> = result
            .diagnostics()
            .iter()
            .filter_map(|it| match *it {
                InferenceDiagnostic::InvalidLhsOfAssignment { assign, op, .. } => Some((assign, op)),
                _ => None,
            })
            .collect();
        assert_eq!(found, [(assign, Some(ArithOp::Add))], "`+=` is E0067");

        let (_, result) = infer(&db, "place");
        assert!(
            !result
                .diagnostics()
                .iter()
                .any(|it| matches!(it, InferenceDiagnostic::InvalidLhsOfAssignment { .. })),
            "a place is assignable"
        );
    });
}

const AMBIGUOUS: &str = r#"
//- minicore: sized
trait Tr {}
impl Tr for u8 {}
impl Tr for u16 {}
fn need<T: Tr>() {}
fn ambiguous() {
    need();
}
fn tainted() {
    let _x: u8 = true;
    need();
}
fn decided() {
    need::<u8>();
}
fn any<U>() {}
fn unconstrained() {
    any();
}
"#;

/// The function and name of the parameter each `TypeMustBeKnown` of a body names as the
/// variable's origin.
fn origins(db: &TestDB, name: &str) -> Vec<Option<(String, String)>> {
    let (_, result) = infer(db, name);
    result
        .diagnostics()
        .iter()
        .filter_map(|it| match it {
            InferenceDiagnostic::TypeMustBeKnown { origin, .. } => Some(origin.as_ref().map(|&(_, param)| {
                let GenericParamId::TypeParamId(param) = param else {
                    panic!("the origin is a type parameter, not {param:?}");
                };
                let GenericDefId::FunctionId(function) = param.parent() else {
                    panic!("the parameter is a function's");
                };
                let params = GenericParams::of(db, param.parent());
                let name = params[param.local_id()].name().map(|it| it.as_str().to_owned());
                (
                    FunctionSignature::of(db, function).name.as_str().to_owned(),
                    name.expect("a named parameter"),
                )
            })),
            _ => None,
        })
        .collect()
}

/// The `TypeMustBeKnown`s of a body: their codes, and the trait of each ambiguous goal.
fn undecided(db: &TestDB, name: &str) -> Vec<(TypeAnnotationNeeded, Option<String>)> {
    let (_, result) = infer(db, name);
    result
        .diagnostics()
        .iter()
        .filter_map(|it| match it {
            InferenceDiagnostic::TypeMustBeKnown { kind, ambiguity, .. } => {
                let trait_ = ambiguity.as_ref().map(|it| match it.primary.goal {
                    StoredGoal::Trait { trait_, .. } => {
                        TraitSignature::of(db, trait_).name.as_str().to_owned()
                    }
                    ref goal => panic!("the goal is a trait's, not {goal:?}"),
                });
                Some((*kind, trait_))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn an_ambiguous_bound_is_e0283_with_its_goal() {
    let db = fixture::db(AMBIGUOUS).into_db();
    crate::attach_db(&db, || {
        // rustc: E0283 "type annotations needed", "cannot satisfy `_: Tr`"; the `T: Sized` of
        // the same variable gives none after it, and writeback none for the variable.
        assert_eq!(
            undecided(&db, "ambiguous"),
            [(TypeAnnotationNeeded::E0283, Some("Tr".to_owned()))]
        );
        // rustc reports no ambiguity of a body with an error (`report_ambiguity_errors`).
        assert!(undecided(&db, "tainted").is_empty());
        assert!(undecided(&db, "decided").is_empty());
    });
}

/// rustc's `maybe_report_ambiguity` ends with `note_obligation_cause`: the ambiguous `_: Tr`
/// of `need()` is required by a bound of `need`.
#[test]
fn an_ambiguity_keeps_why_its_goal_was_required() {
    let db = fixture::db(AMBIGUOUS).into_db();
    crate::attach_db(&db, || {
        let (_, result) = infer(&db, "ambiguous");
        let DefWithBodyId::FunctionId(need) = function(&db, "need") else {
            panic!("`need` is a function");
        };
        let causes: Vec<_> = result
            .diagnostics()
            .iter()
            .filter_map(|it| match it {
                InferenceDiagnostic::TypeMustBeKnown { cause, .. } => Some(*cause),
                _ => None,
            })
            .collect();
        let [ObligationCauseCode::WhereClauseInExpr { item, .. }] = causes[..] else {
            panic!("one ambiguity, required by a bound in `need`, not {causes:?}");
        };
        assert_eq!(item, GenericDefId::FunctionId(need));
    });
}

#[test]
fn an_undecided_variable_names_the_parameter_it_was_made_for() {
    let db = fixture::db(AMBIGUOUS).into_db();
    crate::attach_db(&db, || {
        // rustc: "cannot infer type of the type parameter `T` declared on the function `need`",
        // and the same of `any`'s `U`, which only its `Sized` bound constrains.
        assert_eq!(origins(&db, "ambiguous"), [Some(("need".to_owned(), "T".to_owned()))]);
        assert_eq!(origins(&db, "unconstrained"), [Some(("any".to_owned(), "U".to_owned()))]);
    });
}

#[test]
fn a_mismatch_keeps_an_undecided_literal_type() {
    let db = fixture::db(
        r#"
//- minicore: sized
fn literal() {
    let _x: bool = 1;
}
fn decided() {
    let _x: bool = 1u8;
}
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let at_error = |name| {
            let (_, result) = infer(&db, name);
            let found: Vec<_> = result
                .diagnostics()
                .iter()
                .filter_map(|it| match it {
                    InferenceDiagnostic::TypeMismatch { at_error, .. } => Some(
                        at_error.as_deref().map(|(expected, found)| (expected.as_ref().kind(), found.as_ref().kind())),
                    ),
                    _ => None,
                })
                .collect();
            found
        };
        // rustc: "expected `bool`, found integer"; after fallback the literal is `i32`.
        let types = default_types();
        assert_eq!(
            at_error("literal"),
            [Some((types.types.bool.kind(), TyKind::Infer(InferTy::FreshIntTy(0))))]
        );
        assert_eq!(at_error("decided"), [None]);
    });
}

#[test]
fn a_mismatch_and_an_unmet_bound_say_why_they_were_required() {
    let db = fixture::db(
        r#"
//- minicore: sized, copy
fn else_branch(c: bool) {
    let _ = if c { 1u8 } else { true };
}
fn else_branch_expected(c: bool) -> u8 {
    if c { 1u8 } else { true }
}
fn first_arm(x: u8) -> u8 {
    match x { _ => true }
}
fn later_arm(x: u8) -> u8 {
    match x { 0 => 1u8, 1 => loop {}, _ => true }
}
fn returned() -> u8 {
    return true;
}
fn block_tail() {
    let _: u8 = { true };
}
fn no_else(c: bool) -> u8 {
    if c { 1u8 }
}
fn bare_return() -> u8 {
    return;
}
fn needs<T: Copy>(_: T) {}
struct S;
fn bound() {
    needs(S);
}
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let mismatches = |name| {
            let (_, result) = infer(&db, name);
            result
                .diagnostics()
                .iter()
                .filter_map(|it| match it {
                    InferenceDiagnostic::TypeMismatch { cause, .. } => Some(*cause),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(mismatches("else_branch"), [ObligationCauseCode::IfExpression]);
        // rustc's `check_expr_if` gives the else block the return type as a hard expectation,
        // so the block's own tail is what fails to coerce.
        assert_eq!(mismatches("else_branch_expected"), [ObligationCauseCode::BlockTailExpression]);
        assert_eq!(mismatches("first_arm"), [ObligationCauseCode::BlockTailExpression]);
        // The `!` arm is no prior arm: rustc points at `1u8` alone.
        assert_eq!(mismatches("later_arm"), [ObligationCauseCode::MatchExpressionArm { prior_arms: 1 }]);
        assert_eq!(mismatches("returned"), [ObligationCauseCode::ReturnValue]);
        assert_eq!(mismatches("block_tail"), [ObligationCauseCode::BlockTailExpression]);
        // rustc's E0317 and E0069 causes, of the `()` an `if` without `else` and a bare
        // `return` give.
        assert_eq!(mismatches("no_else"), [ObligationCauseCode::IfExpressionWithNoElse]);
        assert_eq!(mismatches("bare_return"), [ObligationCauseCode::ReturnNoExpression]);

        // rustc: "required by a bound in `needs`".
        let (_, result) = infer(&db, "bound");
        let causes: Vec<_> = result
            .diagnostics()
            .iter()
            .filter_map(|it| match it {
                InferenceDiagnostic::SolverDiagnostic(SolverDiagnostic {
                    cause,
                    kind: SolverDiagnosticKind::TraitUnimplemented { .. },
                    ..
                }) => Some(*cause),
                _ => None,
            })
            .collect();
        let needs = function(&db, "needs").generic_def(&db);
        assert!(
            matches!(
                causes[..],
                [ObligationCauseCode::WhereClauseInExpr { item, arg_index: None, .. }] if item == needs
            ),
            "{causes:?}"
        );
    });
}

/// The causes of the bounds inference left unmet in `name`, in the order it found them.
fn unmet_causes(db: &TestDB, name: &str) -> Vec<ObligationCauseCode> {
    let (_, result) = infer(db, name);
    result
        .diagnostics()
        .iter()
        .filter_map(|it| match it {
            InferenceDiagnostic::SolverDiagnostic(SolverDiagnostic {
                cause,
                kind: SolverDiagnosticKind::TraitUnimplemented { .. },
                ..
            }) => Some(*cause),
            _ => None,
        })
        .collect()
}

/// rustc's "required by this bound in `need`": each `bound_index` of a `WhereClauseInExpr` is
/// found again, a written bound as its syntax and the implicit `Sized` as its parameter.
#[test]
fn a_bound_index_finds_its_written_bound() {
    use syntax::AstNode as _;

    use crate::diagnostics::{BoundSource, bound_source};

    let db = fixture::db(
        r#"
//- minicore: sized, copy, clone
trait Tr {}
fn need<T: Copy + Tr>(_: T) where T: Clone {}
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let DefWithBodyId::FunctionId(need) = function(&db, "need") else { unreachable!() };
        let need = GenericDefId::FunctionId(need);
        let written = |index| match bound_source(&db, need, index) {
            Some(BoundSource::Written(ptr)) => {
                ptr.to_view(&db).to_string()
            }
            other => panic!("bound {index} is written, not {other:?}"),
        };
        assert_eq!([written(0), written(1), written(2)], ["Copy", "Tr", "Clone"]);
        assert!(
            matches!(bound_source(&db, need, 3), Some(BoundSource::ImplicitSized(_))),
            "the fourth is `T`'s implicit `Sized`"
        );
        assert!(bound_source(&db, need, 4).is_none(), "`need` has four bounds");
    });
}

/// rustc's `ty_infer_name_resolver`: an undecided `[T; N]` keeps both variables as their
/// parameters, in printing order ("consider giving `_x` an explicit type: `[T; N]`").
///
/// The one error is the const's: `ConstArgHasType(?N, usize)` sorts before `?T: Sized`, which
/// the taint then drops, so it blames `N` alone (E0284, as the pinned nightly's ui tests
/// `const-generics/infer/issue-77092.stderr` and, for a `let`,
/// `repeat-expr/copy-check-const-element-uninferred-count.stderr`). The binding's type is
/// what names both, as rustc's `FindInferSourceVisitor` prints the `let`'s type.
#[test]
fn an_undecided_term_names_each_variable_by_its_parameter() {
    let db = fixture::db(
        r#"
//- minicore: sized
fn make<T, const N: usize>() -> [T; N] { loop {} }
fn undecided() { let _x = make(); }
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let (body, result) = infer(&db, "undecided");
        let names = |named: &hir_ty::solver_errors::NamedTerm| -> Vec<String> {
            named
                .params
                .iter()
                .map(|&(_, param)| {
                    let param: hir_def::TypeOrConstParamId = match param {
                        GenericParamId::TypeParamId(it) => it.into(),
                        GenericParamId::ConstParamId(it) => it.into(),
                        GenericParamId::LifetimeParamId(_) => {
                            panic!("a lifetime is never undecided")
                        }
                    };
                    let params = GenericParams::of(&db, param.parent);
                    params[param.local_id].name().expect("a named parameter").as_str().to_owned()
                })
                .collect()
        };
        let errors: Vec<_> = result
            .diagnostics()
            .iter()
            .filter_map(|it| match it {
                InferenceDiagnostic::TypeMustBeKnown { kind, named, .. } => Some((*kind, named)),
                _ => None,
            })
            .collect();
        let [(kind, Some(blamed))] = &errors[..] else {
            panic!("one undecided term, the const's: {errors:?}");
        };
        assert_eq!((*kind, names(blamed)), (TypeAnnotationNeeded::E0284, vec!["N".to_owned()]));

        let binding = body
            .pats()
            .find_map(|(id, pat)| match pat {
                hir_def::hir::Pat::Bind { id: binding, .. }
                    if body[binding].name.as_str() == "_x" =>
                {
                    Some(id)
                }
                _ => None,
            })
            .expect("the binding `_x`");
        let named = result.named_pat_type(binding).expect("`_x` is undecided");
        assert_eq!(names(named), ["T", "N"], "both variables of `[T; N]`, the element first");
        let interner = hir_ty::next_solver::DbInterner::new_no_crate(&db);
        let named_all = named.named(interner, |_, _| true);
        assert!(!named_all.has_infer() && !named_all.references_error(), "{named_all:?}");
        assert!(named.named(interner, |_, _| false).references_error(), "unnamed is unknown");
    });
}

/// rustc's "consider giving `x` an explicit type" names every variable of the `let`'s type,
/// not only the error's: writeback keeps each undecided pattern's type named, and a decided
/// one has none.
#[test]
fn an_undecided_pattern_keeps_its_type_with_every_variable_named() {
    let db = fixture::db(
        r#"
//- minicore: sized
fn make<T, U>() -> (T, U) { loop {} }
fn undecided() { let x = make(); let y = 1u8; }
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let (body, result) = infer(&db, "undecided");
        let binding = |name: &str| {
            body.pats()
                .find_map(|(id, pat)| match pat {
                    hir_def::hir::Pat::Bind { id: binding, .. }
                        if body[binding].name.as_str() == name =>
                    {
                        Some(id)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no binding `{name}`"))
        };
        let named = result.named_pat_type(binding("x")).expect("`x` is undecided");
        assert_eq!(named.params.len(), 2, "`T` and `U`, not only the error's own: {named:?}");
        let interner = hir_ty::next_solver::DbInterner::new_no_crate(&db);
        let named_all = named.named(interner, |_, _| true);
        assert!(!named_all.has_infer() && !named_all.references_error(), "{named_all:?}");
        assert!(result.named_pat_type(binding("y")).is_none(), "`y` is `u8`");
    });
}

/// rustc's `report_projection_error`: E0271 keeps what the alias normalizes to ("expected `u8`,
/// found `bool`").
#[test]
fn a_projection_mismatch_keeps_what_its_alias_normalized_to() {
    let db = fixture::db(
        r#"
//- minicore: sized
trait Tr { type A; }
struct S;
impl Tr for S { type A = bool; }
fn need<T: Tr<A = u8>>(_: T) {}
fn mismatch() { need(S); }
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let (_, result) = infer(&db, "mismatch");
        let normalized: Vec<_> = result
            .diagnostics()
            .iter()
            .filter_map(|it| match it {
                InferenceDiagnostic::SolverDiagnostic(SolverDiagnostic {
                    kind: SolverDiagnosticKind::ProjectionMismatch { normalized, .. },
                    ..
                }) => Some(normalized.as_ref().map(|it| it.as_ref().expect_ty().kind())),
                _ => None,
            })
            .collect();
        assert!(
            matches!(normalized[..], [Some(TyKind::Bool)]),
            "one E0271 whose alias normalized to `bool`, not {normalized:?}"
        );
        let impls: Vec<_> = result
            .diagnostics()
            .iter()
            .filter_map(|it| match it {
                InferenceDiagnostic::SolverDiagnostic(SolverDiagnostic {
                    kind: SolverDiagnosticKind::ProjectionMismatch { impl_, .. },
                    ..
                }) => Some(*impl_),
                _ => None,
            })
            .collect();
        assert!(
            matches!(impls[..], [Some(hir_ty::next_solver::AnyImplId::ImplId(_))]),
            "the E0271 names `impl Tr for S`, selected for `S: Tr`, not {impls:?}"
        );
    });
}

/// rustc's `check_expr_unsafe_binder_cast`: `wrap_binder!` into a type that is not an unsafe
/// binder and `unwrap_binder!` of one are each an error naming that type, and the operand of
/// the failed wrap is checked against an error, so no mismatch follows.
#[test]
fn an_unsafe_binder_cast_of_a_type_that_is_no_binder_is_an_error() {
    let db = fixture::db(
        r#"
//- minicore: sized
fn casts(x: &i32) {
    let _ = unsafe { builtin # wrap_binder(x, u8) };
    let _ = unsafe { builtin # unwrap_binder(1u8) };
}
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let (_, result) = infer(&db, "casts");
        let casts: Vec<_> = result
            .diagnostics()
            .iter()
            .filter_map(|it| match it {
                InferenceDiagnostic::UnsafeBinderCastOfNonBinder { kind, found, .. } => {
                    Some((*kind, found.as_ref().kind()))
                }
                _ => None,
            })
            .collect();
        assert!(
            matches!(
                casts[..],
                [
                    (hir_def::hir::UnsafeBinderCastKind::Wrap, TyKind::Uint(_)),
                    (hir_def::hir::UnsafeBinderCastKind::Unwrap, TyKind::Uint(_)),
                ]
            ),
            "the wrap into `u8`, then the unwrap of `u8`, not {casts:?}"
        );
        assert!(
            !result
                .diagnostics()
                .iter()
                .any(|it| matches!(it, InferenceDiagnostic::TypeMismatch { .. })),
            "{:?}",
            result.diagnostics()
        );
    });
}

/// rustc's E0277 for an unsized parameter, return type and local (`check_fn`,
/// `GatherLocalsVisitor`; `tests/ui/unsized-locals/issue-30276.rs`, `issue-50940.rs`).
#[test]
fn parameters_return_types_and_bindings_must_be_sized() {
    let db = fixture::db(
        r#"
//- minicore: sized
fn sized_param(x: [u8]) {}
fn sized_ret() -> str { loop {} }
fn sized_binding(s: &str) { let t = *s; }
fn sized_ok(x: &[u8]) {}
fn sized_closure_param() { let _ = |x: [u8]| {}; }
fn sized_two(x: [u8], s: &str) { let t = *s; }
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        use ObligationCauseCode as C;
        assert_eq!(unmet_causes(&db, "sized_param"), [C::SizedArgumentType]);
        assert_eq!(unmet_causes(&db, "sized_ret"), [C::SizedReturnType]);
        assert_eq!(unmet_causes(&db, "sized_binding"), [C::LetType]);
        assert_eq!(unmet_causes(&db, "sized_ok"), []);
        assert_eq!(unmet_causes(&db, "sized_closure_param"), [C::SizedArgumentType]);
        // rustc's order: the parameter, then the local.
        assert_eq!(unmet_causes(&db, "sized_two"), [C::SizedArgumentType, C::LetType]);
    });
}

/// A fn with no body asks nothing `Sized`: rustc's parameter check is typeck's `check_fn` and
/// its return check wfcheck's "if the function has a body" (stdx's `TupleExt::head(self)` on
/// a `?Sized` trait, which blaze once refused).
#[test]
fn a_fn_without_a_body_needs_no_sized_parameters_or_return_type() {
    let db = fixture::db(
        r#"
//- minicore: sized
trait Head {
    fn head(self) -> Self;
    fn slice(x: [u8]) -> str;
}
extern "C" {
    fn foreign(x: [u8]) -> str;
}
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        let mut methods = Vec::new();
        for &krate in base_db::all_crates(&db).iter() {
            let def_map = hir_def::nameres::crate_def_map(&db, krate);
            for (_, module) in def_map.modules(&db) {
                for def in module.scope(&db).declarations() {
                    if let hir_def::ModuleDefId::TraitId(it) = def
                        && TraitSignature::of(&db, it).name.as_str() == "Head"
                    {
                        for &(_, item) in it.trait_items(&db).items.iter() {
                            if let hir_def::AssocItemId::FunctionId(f) = item {
                                methods.push(DefWithBodyId::FunctionId(f));
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(methods.len(), 2, "`head` and `slice`");
        methods.push(function(&db, "foreign"));
        for def in methods {
            let result = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
            assert!(result.diagnostics().is_empty(), "{def:?}: {:?}", result.diagnostics());
        }
    });
}

/// `unsized_fn_params` lifts a fn's parameters but not a closure's (rustc's
/// `tests/ui/unsized-locals/issue-67981.rs`) nor a local's.
#[test]
fn unsized_fn_params_lifts_only_a_fns_parameters() {
    let db = fixture::db(
        r#"
//- minicore: sized
//- /main.rs crate:main
#![feature(unsized_fn_params)]
fn unsized_param(x: [u8]) {}
fn unsized_closure_param() { let _ = |x: [u8]| {}; }
fn unsized_binding(s: &str) { let t = *s; }
"#,
    )
    .into_db();
    crate::attach_db(&db, || {
        use ObligationCauseCode as C;
        assert_eq!(unmet_causes(&db, "unsized_param"), []);
        assert_eq!(unmet_causes(&db, "unsized_closure_param"), [C::SizedArgumentType]);
        assert_eq!(unmet_causes(&db, "unsized_binding"), [C::LetType]);
    });
}

/// rustc's `tests/ui/label/label_break_value_unlabeled_break.rs` and
/// `label_break_value_continue.rs`, with a `try` block, whose label is desugared, and a
/// closure between the jump and the labeled block.
const JUMPS: &str = r#"
//- minicore: sized, try, option
//- /main.rs crate:main edition:2021
#![feature(try_blocks)]
fn break_simple() {
    'b: {
        break;
    }
}
fn break_crossing() {
    loop {
        'b: {
            break;
        }
    }
}
fn continue_crossing() {
    loop {
        'b: {
            continue;
        }
    }
}
fn labeled() {
    loop {
        'b: {
            break 'b;
        }
    }
}
fn in_inner_loop() {
    'b: {
        loop {
            break;
        }
    }
}
fn in_try() {
    loop {
        let _: Option<()> = try {
            break;
        };
    }
}
fn in_closure() {
    'b: {
        let _ = || loop {
            break;
        };
    }
}
"#;

#[test]
fn an_unlabeled_jump_in_a_labeled_block_is_e0695() {
    let db = fixture::db(JUMPS).into_db();
    crate::attach_db(&db, || {
        let jumps = |name: &str| -> Vec<bool> {
            let (_, result) = infer(&db, name);
            result
                .diagnostics()
                .iter()
                .filter_map(|it| match *it {
                    InferenceDiagnostic::UnlabeledJumpInLabeledBlock { is_break, .. } => Some(is_break),
                    _ => None,
                })
                .collect()
        };
        let outside = |name: &str| {
            let (_, result) = infer(&db, name);
            result.diagnostics().iter().any(|it| matches!(it, InferenceDiagnostic::BreakOutsideOfLoop { .. }))
        };
        assert_eq!(jumps("break_simple"), [true]);
        assert!(!outside("break_simple"), "only E0695, as rustc gives");
        assert_eq!(jumps("break_crossing"), [true]);
        assert_eq!(jumps("continue_crossing"), [false]);
        assert!(!outside("continue_crossing"));
        for name in ["labeled", "in_inner_loop", "in_try", "in_closure"] {
            assert_eq!(jumps(name), Vec::<bool>::new(), "{name}");
            assert!(!outside(name), "{name}");
        }
    });
}

/// rustc's `tests/ui/coroutine/async-coroutine-issue-67158.rs`.
const ASYNC_YIELD: &str = r#"
//- minicore: sized, future
//- /main.rs crate:main edition:2021
#![feature(coroutines)]
fn in_async_block() {
    let _ = async { yield 1u8 };
}
async fn in_async_fn() {
    yield;
}
fn in_closure_in_async() {
    let _ = async {
        let _ = #[coroutine] || {
            yield 1u8;
        };
    };
}
"#;

#[test]
fn a_yield_in_an_async_body_is_e0727() {
    let db = fixture::db(ASYNC_YIELD).into_db();
    crate::attach_db(&db, || {
        let count = |name: &str| {
            let (body, result) = infer(&db, name);
            let found: Vec<ExprId> = result
                .diagnostics()
                .iter()
                .filter_map(|it| match *it {
                    InferenceDiagnostic::AsyncCoroutinesNotSupported { expr } => Some(expr),
                    InferenceDiagnostic::YieldOutsideCoroutine { .. } => panic!("{name}: not E0627"),
                    _ => None,
                })
                .collect();
            for &it in &found {
                assert!(matches!(body.expr(it), Expr::Yield { .. }), "{name}: on the `yield`");
            }
            found.len()
        };
        assert_eq!(count("in_async_block"), 1);
        assert_eq!(count("in_async_fn"), 1);
        assert_eq!(count("in_closure_in_async"), 0, "a `#[coroutine]` closure may yield");
    });
}

/// rustc's `tests/ui/reachable/`: `expr_block.rs`, `unreachable-in-call.rs`, `expr_if.rs`,
/// `expr_match.rs` and `unwarned-match-on-never.rs`, with no macros.
const REACHABLE: &str = r#"
//- minicore: sized, try, option
fn diverge() -> ! { loop {} }
fn get_u8() -> u8 { 1 }
fn call(_: u8, _: u8) {}
fn tail() {
    let _x = {
        return;
        22
    };
}
fn diverge_first() {
    call(diverge(), get_u8());
}
fn diverge_second() {
    call(get_u8(), diverge());
}
fn statements() {
    return;
    let _x = 1;
    let _y = 2;
}
fn if_condition() {
    if { return } {}
}
fn arms_diverge() {
    match () { () if false => return, () => return }
    let _x = 1;
}
fn an_arm_converges() {
    match () { () if false => return, () => () }
    let _x = 1;
}
fn scrutinee() {
    match (return) {
        () => ()
    }
}
fn never(x: !) -> bool {
    match x {}
}
fn question(x: Option<u8>) -> Option<u8> {
    let y = x?;
    Some(y)
}
"#;

#[test]
fn type_checking_finds_where_a_body_stops_being_reachable() {
    let db = fixture::db(REACHABLE).into_db();
    crate::attach_db(&db, || {
        let found = |name: &str| -> (&Body, Vec<Unreachable>) {
            let (body, result) = infer(&db, name);
            (body, result.unreachable().to_vec())
        };
        let after_return = |body: &Body, it: &Unreachable| match it.after {
            ExprOrPatId::ExprId(it) => matches!(body.expr(it), Expr::Return { .. }),
            ExprOrPatId::PatId(_) => false,
        };

        // "unreachable expression": the tail after `return;`.
        let (body, all) = found("tail");
        let literal = find_expr(&body, |it| matches!(it, Expr::Literal(_)));
        assert_eq!(all.iter().map(|it| it.what).collect::<Vec<_>>(), [UnreachableAt::Expr(literal)]);
        assert!(after_return(&body, &all[0]));
        assert_eq!(all[0].note, None);

        // "unreachable expression": an argument after a diverging one, and not the call too.
        let (body, all) = found("diverge_first");
        let outer = find_expr(&body, |it| matches!(it, Expr::Call { args, .. } if args.len() == 2));
        let Expr::Call { args, .. } = body.expr(outer) else { unreachable!() };
        let (first, second) = (args[0], args[1]);
        assert_eq!(all.iter().map(|it| it.what).collect::<Vec<_>>(), [UnreachableAt::Expr(second)]);
        assert_eq!(all[0].after, ExprOrPatId::ExprId(first));

        // "unreachable call": the last argument diverged.
        let (body, all) = found("diverge_second");
        let outer = find_expr(&body, |it| matches!(it, Expr::Call { args, .. } if args.len() == 2));
        let Expr::Call { args, .. } = body.expr(outer) else { unreachable!() };
        assert_eq!(all.iter().map(|it| it.what).collect::<Vec<_>>(), [UnreachableAt::Call(outer)]);
        assert_eq!(all[0].after, ExprOrPatId::ExprId(args[1]));

        // "unreachable statement": the first after `return;`, once.
        let (body, all) = found("statements");
        assert_eq!(
            all.iter().map(|it| it.what).collect::<Vec<_>>(),
            [UnreachableAt::Stmt { block: body.root_expr(), index: 1 }]
        );
        assert!(after_return(&body, &all[0]));

        // "unreachable block in `if` or `while` expression".
        let (body, all) = found("if_condition");
        let if_ = find_expr(&body, |it| matches!(it, Expr::If { .. }));
        assert_eq!(all.iter().map(|it| it.what).collect::<Vec<_>>(), [UnreachableAt::IfBlock(if_)]);
        assert!(after_return(&body, &all[0]));

        // A `match` whose arms all diverge, a guard not counted.
        let (body, all) = found("arms_diverge");
        let match_ = find_expr(&body, |it| matches!(it, Expr::Match { .. }));
        assert_eq!(
            all,
            [Unreachable {
                what: UnreachableAt::Stmt { block: body.root_expr(), index: 1 },
                after: ExprOrPatId::ExprId(match_),
                note: Some(Unreachable::ALL_ARMS_DIVERGE),
            }]
        );
        assert_eq!(found("an_arm_converges").1, []);

        // "unreachable arm": the scrutinee diverged.
        let (body, all) = found("scrutinee");
        let match_ = find_expr(&body, |it| matches!(it, Expr::Match { .. }));
        assert_eq!(
            all.iter().map(|it| it.what).collect::<Vec<_>>(),
            [UnreachableAt::Arm { match_expr: match_, index: 0 }]
        );
        assert!(after_return(&body, &all[0]));

        // An explicit match on `!` is quiet, and so is the `match` of a desugared `?`.
        assert_eq!(found("never").1, []);
        assert_eq!(found("question").1, []);
    });
}

/// rustc's `binop/issue-93927.rs` and `[X; 3] == [X; 3]` (minicore has no array `PartialEq`,
/// so a local wrapper with the same impl), and `-X`.
const OPERATORS: &str = r#"
//- minicore: sized, eq, unary_ops
struct X;
struct W<T>(T);
impl<T: PartialEq> PartialEq for W<T> {
    fn eq(&self, _: &W<T>) -> bool { true }
}
fn nested() {
    let _b: bool = W(X) == W(X);
}
fn own() {
    let _ = X == X;
}
fn neg() {
    let _ = -X;
}
"#;

#[test]
fn an_operator_error_carries_its_unmet_goals() {
    let db = fixture::db(OPERATORS).into_db();
    crate::attach_db(&db, || {
        // Each goal as `Trait` and the name of its self type.
        let names = |goals: &[StoredGoal]| -> Vec<(String, String)> {
            goals
                .iter()
                .map(|goal| {
                    let StoredGoal::Trait { trait_, args, .. } = goal else {
                        panic!("the goal is a trait's, not {goal:?}")
                    };
                    let trait_ = TraitSignature::of(&db, *trait_).name.as_str().to_owned();
                    let self_ty = match args.as_ref().type_at(0).kind() {
                        TyKind::Adt(adt, _) => match adt.def_id() {
                            AdtId::StructId(it) => StructSignature::of(&db, it).name.as_str().to_owned(),
                            it => panic!("the self type is a struct, not {it:?}"),
                        },
                        it => panic!("the self type is an ADT, not {it:?}"),
                    };
                    (trait_, self_ty)
                })
                .collect()
        };
        let binary = |name: &str| {
            let (_, result) = infer(&db, name);
            let found: Vec<_> = result
                .diagnostics()
                .iter()
                .filter_map(|it| match it {
                    InferenceDiagnostic::BinaryOperatorCannotBeApplied { unmet, output, .. } => {
                        Some((names(unmet), output.as_ref().map(|it| it.as_ref().kind())))
                    }
                    _ => None,
                })
                .collect();
            found
        };
        let pair = |trait_: &str, self_ty: &str| (trait_.to_owned(), self_ty.to_owned());
        let types = default_types();

        // rustc: "an implementation of `PartialEq` might be missing for `X`", from the leaf
        // goal of `W<X>: PartialEq`, and the `bool` the comparison is expected to be.
        assert_eq!(
            binary("nested"),
            [(vec![pair("PartialEq", "X")], Some(types.types.bool.kind()))]
        );
        let own: Vec<_> = binary("own").into_iter().map(|(goals, _)| goals).collect();
        assert_eq!(own, [vec![pair("PartialEq", "X")]], "the trait's own goal is its leaf");

        let (_, result) = infer(&db, "neg");
        let found: Vec<_> = result
            .diagnostics()
            .iter()
            .filter_map(|it| match it {
                InferenceDiagnostic::UnaryOperatorCannotBeApplied { unmet, .. } => Some(names(unmet)),
                _ => None,
            })
            .collect();
        assert_eq!(found, [vec![pair("Neg", "X")]]);
    });
}

/// rustc's `binop/binop-typeck.rs` ("cannot add `{integer}` to `bool`"), E0614 of `*1` and
/// E0600 of `!1.0`: the literal's type is undecided when the error is found.
const AT_ERROR: &str = r#"
//- minicore: sized, add, unary_ops
fn add() {
    let _ = true + 1;
}
fn decided() {
    let _ = true + 1u8;
}
fn deref() {
    let _ = *1;
}
fn not() {
    let _ = !1.0;
}
"#;

#[test]
fn an_operator_error_keeps_an_undecided_literal_type() {
    let db = fixture::db(AT_ERROR).into_db();
    crate::attach_db(&db, || {
        let types = default_types();
        let fresh_int = TyKind::Infer(InferTy::FreshIntTy(0));
        let at_error = |name: &str| -> Vec<Vec<TyKind<'_>>> {
            let (_, result) = infer(&db, name);
            result
                .diagnostics()
                .iter()
                .filter_map(|it| match it {
                    InferenceDiagnostic::BinaryOperatorCannotBeApplied { at_error, .. } => {
                        Some(at_error.as_deref().map_or(Vec::new(), |(lhs, rhs)| {
                            vec![lhs.as_ref().kind(), rhs.as_ref().kind()]
                        }))
                    }
                    InferenceDiagnostic::CannotBeDereferenced { at_error, .. }
                    | InferenceDiagnostic::UnaryOperatorCannotBeApplied { at_error, .. } => {
                        Some(at_error.iter().map(|it| it.as_ref().kind()).collect())
                    }
                    _ => None,
                })
                .collect()
        };
        assert_eq!(at_error("add"), [vec![types.types.bool.kind(), fresh_int]]);
        assert_eq!(at_error("decided"), [Vec::new()], "no literal undecided: `None`");
        assert_eq!(at_error("deref"), [vec![fresh_int]]);
        assert_eq!(at_error("not"), [vec![TyKind::Infer(InferTy::FreshFloatTy(0))]]);
    });
}

/// rustc's `argument-suggestions/extra_arguments.rs`, `swapped_arguments.rs` and E0057: what
/// `report_arg_errors` is given of each call.
const ARGUMENTS: &str = r#"
//- minicore: sized, fn
fn one(_a: u8) {}
fn two(_a: u8, _b: &str) {}
struct S;
impl S {
    fn m(&self, _a: u8, _b: &str) {}
}
fn count() {
    one(1, 2);
}
fn swapped() {
    two("", 1);
}
fn method() {
    S.m("", 1);
}
fn closure() {
    let f = |_a: u8| {};
    f(1, 2);
}
fn fits() {
    two(1, "");
}
"#;

#[test]
fn a_call_whose_arguments_do_not_fit_carries_its_argument_matrix() {
    let db = fixture::db(ARGUMENTS).into_db();
    crate::attach_db(&db, || {
        let types = default_types();
        let fresh_int = TyKind::Infer(InferTy::FreshIntTy(0));
        // (whether the count fits, is_fn_trait_call, the call's `CallArguments`)
        let calls = |name: &str| {
            let (_, result) = infer(&db, name);
            result
                .diagnostics()
                .iter()
                .filter_map(|it| match it {
                    InferenceDiagnostic::MismatchedArgCount { is_fn_trait_call, args, .. } => {
                        Some((false, *is_fn_trait_call, (**args).clone()))
                    }
                    InferenceDiagnostic::IncorrectArguments { args, .. } => {
                        Some((true, false, (**args).clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let rows = |args: &hir_ty::CallArguments| {
            (0..args.provided.len()).map(|it| args.compatible_with(it).to_vec()).collect::<Vec<_>>()
        };

        let count = calls("count");
        let [(fits, trait_call, args)] = &count[..] else { panic!("one count error") };
        assert!(!fits && !trait_call && !args.is_method && args.method.is_none());
        assert_eq!(args.formal_inputs.len(), 1);
        let provided: Vec<_> = args.provided.iter().map(|it| it.as_ref().kind()).collect();
        assert_eq!(provided, [types.types.u8.kind(), fresh_int], "`2` is undecided at the error");
        assert_eq!(rows(args), [vec![0], vec![0]], "either argument fits `u8`");

        for name in ["swapped", "method"] {
            let found = calls(name);
            let [(fits, _, args)] = &found[..] else { panic!("one error of {name}") };
            assert!(*fits, "{name}: the count fits");
            assert_eq!(args.is_method, name == "method");
            assert_eq!(args.method.is_some(), name == "method");
            assert_eq!(args.provided[1].as_ref().kind(), fresh_int);
            assert_eq!(rows(args), [vec![1], vec![0]], "{name}: each fits only the other's input");
        }

        let closure = calls("closure");
        let [(fits, trait_call, args)] = &closure[..] else { panic!("one E0057") };
        assert!(!fits && *trait_call && !args.is_method);
        assert!(matches!(args.callee.as_ref().kind(), TyKind::Closure(..)));
        assert_eq!(rows(args), [vec![0], vec![0]]);

        assert!(calls("fits").is_empty());
    });
}

/// rustc's `error-codes/E0641.rs` (`0 as *const _`), a function pointer cast to a pointer of
/// unknown pointee, and `cast/cast-enum-with-dtor.rs`'s field-less enum with a `Drop` impl.
const CASTS: &str = r#"
//- minicore: sized, drop
enum E { A }
impl Drop for E { fn drop(&mut self) {} }
enum F { A }
fn from_int() {
    let _p = 0 as *const _;
}
fn from_fn_ptr(f: fn()) {
    let _p = f as *const _;
}
fn known(f: fn()) {
    let _p = 0 as *const u8;
    let _q = f as *const u8;
}
fn enum_drop() {
    let _ = E::A as u8;
}
fn enum_plain() {
    let _ = F::A as u8;
}
"#;

#[test]
fn a_cast_to_a_pointer_of_unknown_kind_is_e0641() {
    let db = fixture::db(CASTS).into_db();
    crate::attach_db(&db, || {
        let errors = |name: &str| -> Vec<CastError> {
            let (_, result) = infer(&db, name);
            result
                .diagnostics()
                .iter()
                .filter_map(|it| match *it {
                    InferenceDiagnostic::InvalidCast { error, .. } => Some(error),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(errors("from_int"), [CastError::UnknownCastPtrKind]);
        assert_eq!(errors("from_fn_ptr"), [CastError::UnknownCastPtrKind]);
        assert_eq!(errors("known"), []);
        assert_eq!(errors("enum_drop"), [CastError::CastEnumDrop]);
        assert_eq!(errors("enum_plain"), []);
    });
}

/// rustc's `match/match-range-fail.rs` and the tuple struct of #67037.
const PATTERNS: &str = r#"
//- minicore: sized
fn one_bad_end() {
    match "wow" {
        10 ..= "what" => (),
    };
}
fn both_bad() {
    match "wow" {
        "bar" ..= "foo" => {}
    };
}
struct W((u8, u8));
fn count_of_other_type() {
    let W(_a, _b) = 0u8;
}
fn count() {
    let W(_a, _b) = W((1, 2));
}
"#;

#[test]
fn pattern_errors_carry_what_rustc_reports() {
    let db = fixture::db(PATTERNS).into_db();
    crate::attach_db(&db, || {
        let ranges = |name: &str| -> Vec<(Option<(TyKind<'_>, bool)>, Option<bool>)> {
            let (_, result) = infer(&db, name);
            result
                .diagnostics()
                .iter()
                .filter_map(|it| match it {
                    InferenceDiagnostic::InvalidRangePatType { lhs, rhs, .. } => Some((
                        lhs.as_ref().map(|(ty, fail)| (ty.as_ref().kind(), *fail)),
                        rhs.as_ref().map(|&(_, fail)| fail),
                    )),
                    _ => None,
                })
                .collect()
        };
        // "this is of type `{integer}`" on `10`, "... `&'static str` but it should be `char` or
        // numeric" on `"what"`.
        assert_eq!(
            ranges("one_bad_end"),
            [(Some((TyKind::Infer(InferTy::FreshIntTy(0)), false)), Some(true))]
        );
        let both = ranges("both_bad");
        assert_eq!(both.len(), 1);
        assert_eq!((both[0].0.as_ref().map(|(_, fail)| *fail), both[0].1), (Some(true), Some(true)));

        let counts = |name: &str| -> Vec<bool> {
            let (_, result) = infer(&db, name);
            result
                .diagnostics()
                .iter()
                .filter_map(|it| match *it {
                    InferenceDiagnostic::MismatchedTupleStructPatArgCount { had_err, .. } => {
                        Some(had_err)
                    }
                    _ => None,
                })
                .collect()
        };
        // rustc suggests `W((_a, _b))` only where the pattern's type matched.
        assert_eq!(counts("count_of_other_type"), [true]);
        assert_eq!(counts("count"), [false]);
    });
}

const VALUES: &str = r#"
//- minicore: sized
static mut STATIC_MUT: u8 = 0;
struct P { f: u8 }
const C: P = P { f: 1 };
fn values() {
    let local = P { f: 2 };
    let _a = unsafe { &STATIC_MUT };
    let _b = &local.f;
    let ref _c = C;
}
"#;

#[test]
fn inference_records_the_value_each_path_resolved_to() {
    let db = fixture::db(VALUES).into_db();
    crate::attach_db(&db, || {
        let (body, result) = infer(&db, "values");
        let values: Vec<_> = body
            .exprs()
            .filter(|(_, it)| matches!(it, Expr::Path(_)))
            .map(|(id, _)| result.value_resolution(id.into()))
            .collect();
        let [Some(ValueNs::StaticId(_)), Some(ValueNs::LocalBinding(local)), Some(ValueNs::ConstId(_))] =
            values[..]
        else {
            panic!("`STATIC_MUT`, `local` and `C` resolve through inference, not {values:?}");
        };
        assert_eq!(body[local].name.as_str(), "local");
    });
}

/// rustc's `issues/issue-35976.rs`: `Future` is not in scope, so `wait` is the trait object's,
/// and a probe of every trait picks `Future`'s for `B` instead.
const SIZED_BOUND: &str = r#"
//- minicore: sized, receiver
mod private {
    pub trait Future {
        fn wait(&self) where Self: Sized;
    }
    pub struct B(pub &'static dyn Future);
    impl core::ops::Deref for B {
        type Target = dyn Future;
        fn deref(&self) -> &(dyn Future + 'static) { self.0 }
    }
    impl Future for B {
        fn wait(&self) {}
    }
}
fn bar(arg: private::B) {
    arg.wait();
}
"#;

#[test]
fn a_sized_bound_on_a_trait_object_carries_the_pick() {
    let db = fixture::db(SIZED_BOUND).into_db();
    crate::attach_db(&db, || {
        let (_, result) = infer(&db, "bar");
        let found: Vec<_> = result
            .diagnostics()
            .iter()
            .filter_map(|it| match it {
                InferenceDiagnostic::MethodCallIllegalSizedBound {
                    method, bound, needs_mut, candidates, ..
                } => Some((*method, *bound, *needs_mut, candidates.clone())),
                _ => None,
            })
            .collect();
        let [(method, bound, needs_mut, ref candidates)] = found[..] else {
            panic!("one illegal `Sized` bound of `wait`, not {found:?}");
        };
        let ItemContainerId::TraitId(future) = method.lookup(&db).container else {
            panic!("`wait` is the trait's method");
        };
        assert_eq!(TraitSignature::of(&db, future).name.as_str(), "Future");
        assert_eq!(bound, Some(method.into()), "the bound is in `wait`'s where clause");
        assert!(!needs_mut);
        assert_eq!(&candidates[..], [future], "a probe of every trait picks `Future`'s `wait` for `B`");
    });
}

/// rustc's `unstable_name_collisions`: the probe passes over `S::gated`, whose feature `main`
/// does not enable, and picks the trait's.
const UNSTABLE: &str = r#"
//- minicore: sized
//- /main.rs crate:main deps:dep
trait Ext {
    fn gated(&self) -> u8;
}
impl Ext for dep::S {
    fn gated(&self) -> u8 { 0 }
}
fn call(s: dep::S) {
    s.gated();
}
fn path(s: dep::S) {
    dep::S::gated(&s);
}
fn field(s: dep::S) {
    let _ = s.gated;
}
//- /lib.rs crate:dep
pub struct S;
impl S {
    #[unstable(feature = "gated", issue = "none")]
    pub fn gated(&self) -> u16 { 0 }
}
"#;

#[test]
fn a_probe_records_the_unstable_items_it_passed_over() {
    let db = fixture::db(UNSTABLE).into_db();
    crate::attach_db(&db, || {
        for name in ["call", "path"] {
            let (body, result) = infer(&db, name);
            let call = find_expr(&body, |it| matches!(it, Expr::MethodCall { .. } | Expr::Call { .. }));
            let node = match body.expr(call) {
                Expr::Call { callee, .. } => callee,
                _ => call,
            };
            let candidates = result.unstable_candidates(node.into());
            let [(CandidateId::FunctionId(passed), ref feature)] = *candidates else {
                panic!("`{name}` passed over `S::gated`, not {candidates:?}");
            };
            assert_eq!(feature.as_str(), "gated");
            let ItemContainerId::ImplId(_) = passed.lookup(&db).container else {
                panic!("the passed-over `gated` is the inherent one");
            };
        }
    });
}

/// rustc's `method_exists_for_diagnostic`: a field access that names no field probes for a
/// method, and its pick's unstable candidates and the method are kept under the field
/// expression, which `unstable_candidate_nodes` lists alone.
#[test]
fn a_field_naming_a_method_keeps_its_probe_s_unstable_items() {
    let db = fixture::db(UNSTABLE).into_db();
    crate::attach_db(&db, || {
        let (body, result) = infer(&db, "field");
        let field = find_expr(&body, |it| matches!(it, Expr::Field { .. }));
        let nodes: Vec<_> = result.unstable_candidate_nodes().collect();
        let [(node, [(CandidateId::FunctionId(passed), feature)])] = &nodes[..] else {
            panic!("only the field access passed over `S::gated`, not {nodes:?}");
        };
        assert_eq!(*node, ExprOrPatId::ExprId(field));
        assert_eq!(feature.as_str(), "gated");
        let ItemContainerId::ImplId(_) = passed.lookup(&db).container else {
            panic!("the passed-over `gated` is the inherent one");
        };
        let (picked, _) = result.method_resolution(field).expect("the probe picked a method");
        let ItemContainerId::TraitId(_) = picked.lookup(&db).container else {
            panic!("the picked `gated` is `Ext`'s");
        };
    });
}

/// rustc's `lint_edition_dependent_dot_call` and `lint_fully_qualified_call_from_2018`: the 2021
/// prelude's `TryInto` and `TryFrom` would make a 2018 crate's own `try_into` and `try_from`
/// ambiguous.
const EDITIONS: &str = r#"
//- minicore: sized
//- /main.rs crate:main edition:2018 deps:new
trait MyTryInto {
    fn try_into(self) -> u8;
}
impl MyTryInto for u16 {
    fn try_into(self) -> u8 { 0 }
}
trait MyTryFrom: Sized {
    fn try_from(x: i32) -> Self;
}
impl MyTryFrom for u8 {
    fn try_from(_: i32) -> Self { 0 }
}
fn old_call() {
    let _ = 1u16.try_into();
}
fn old_path() {
    let _ = u8::try_from(1);
}
//- /lib.rs crate:new edition:2021
trait MyTryInto {
    fn try_into(self) -> u8;
}
impl MyTryInto for u16 {
    fn try_into(self) -> u8 { 0 }
}
fn new_call() {
    let _ = 1u16.try_into();
}
"#;

#[test]
fn a_2018_crate_records_the_calls_the_2021_prelude_makes_ambiguous() {
    let db = fixture::db(EDITIONS).into_db();
    crate::attach_db(&db, || {
        let lints = |name: &str| -> Vec<InferenceDiagnostic> {
            let (_, result) = infer(&db, name);
            let lint = |it: &&InferenceDiagnostic| {
                matches!(
                    it,
                    InferenceDiagnostic::EditionDependentMethodCall { .. }
                        | InferenceDiagnostic::FullyQualifiedCallFrom2018 { .. }
                )
            };
            result.diagnostics().iter().filter(lint).cloned().collect()
        };

        let call = lints("old_call");
        let [
            InferenceDiagnostic::EditionDependentMethodCall {
                lint: EditionLint::Rust2021PreludeCollisions,
                method,
                inherent: false,
                autoderefs: 0,
                autoref: None,
                ..
            },
        ] = call[..]
        else {
            panic!("`1u16.try_into()` is linted as a 2021 prelude collision, not {call:?}");
        };
        let ItemContainerId::TraitId(_) = method.lookup(&db).container else {
            panic!("a trait pick is the trait's `try_into`");
        };

        let path = lints("old_path");
        let [InferenceDiagnostic::FullyQualifiedCallFrom2018 { item: CandidateId::FunctionId(_), .. }] =
            path[..]
        else {
            panic!("`u8::try_from` is linted as a 2021 prelude collision, not {path:?}");
        };

        assert!(lints("new_call").is_empty(), "a 2021 crate has the new prelude already");
    });
}

/// rustc's late resolver on paths of more than one segment (`typeck/path-to-method-sugg-unresolved-expr.rs`).
const UNRESOLVED_PATHS: &str = r#"
//- minicore: sized
mod a {
    pub fn f() {}
    pub struct S { pub x: u8 }
    pub struct T(pub u8);
    pub struct B { pub x: u8 }
}
struct Ty;
impl Ty {}
fn prefix() { let _n = page_size::get(); }
fn middle() { a::nothere::f(); }
fn last() { a::nothere(); }
fn found() { a::f(); }
fn assoc() { Ty::nope(); }
fn other_namespace() { let _b = a::B; }
fn literal() { let _q = a::Q { x: 1 }; }
fn literal_found() { let _s = a::S { x: 1 }; }
fn single_literal() { let _n = Nope {}; }
fn tuple_pattern(v: a::T) { let a::U(_) = v; }
fn record_pattern(v: a::S) { let a::R { .. } = v; }
fn ident() { nope; }
"#;

#[test]
fn a_path_that_names_nothing_reports_its_first_failing_segment() {
    let db = fixture::db(UNRESOLVED_PATHS).into_db();
    crate::attach_db(&db, || {
        let segments = |name: &str| -> Vec<u32> {
            let (_, result) = infer(&db, name);
            result
                .diagnostics()
                .iter()
                .filter_map(|it| match *it {
                    InferenceDiagnostic::UnresolvedPath { segment, .. } => Some(segment),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(segments("prefix"), [0], "`page_size` is E0433");
        assert_eq!(segments("middle"), [1], "`nothere` in `a` is E0433");
        assert_eq!(segments("last"), [1], "the last segment is E0425");
        assert_eq!(segments("found"), Vec::<u32>::new());
        assert_eq!(segments("assoc"), Vec::<u32>::new(), "an unknown associated item is E0599's");
        assert_eq!(segments("other_namespace"), Vec::<u32>::new(), "a struct used as a value is E0423's");
        assert_eq!(segments("literal"), [1]);
        assert_eq!(segments("literal_found"), Vec::<u32>::new());
        assert_eq!(segments("single_literal"), [0]);
        assert_eq!(segments("tuple_pattern"), [1]);
        assert_eq!(segments("record_pattern"), [1]);
        assert_eq!(segments("ident"), Vec::<u32>::new(), "one identifier stays `UnresolvedIdent`");
        let (_, ident) = infer(&db, "ident");
        assert!(
            ident
                .diagnostics()
                .iter()
                .any(|it| matches!(it, InferenceDiagnostic::UnresolvedIdent { .. }))
        );
    });
}

const UNSTABLE_METHOD: &str = r#"
//- minicore: sized
//- /iterator.rs crate:iterator edition:2015 library
#![feature(staged_api)]
#![stable(feature = "ipu_iterator", since = "1.0.0")]
#[stable(feature = "ipu_iterator", since = "1.0.0")]
pub trait IpuIterator {
    #[unstable(feature = "ipu_flatten", issue = "99999")]
    fn ipu_flatten(&self) -> u32 { 0 }
    #[unstable(feature = "assoc_const_ipu_iter", issue = "99999")]
    const C: i32;
}
#[stable(feature = "ipu_iterator", since = "1.0.0")]
impl IpuIterator for char {
    const C: i32 = 42;
}
//- /itertools.rs crate:itertools edition:2015 library
pub trait IpuItertools {
    fn ipu_flatten(&self) -> u32 { 1 }
    const C: i32;
}
impl IpuItertools for char {
    const C: i32 = 1;
}
//- /main.rs crate:main edition:2015 deps:iterator,itertools
use iterator::IpuIterator;
use itertools::IpuItertools;
fn call() -> u32 {
    'x'.ipu_flatten()
}
fn path() -> i32 {
    char::C
}
"#;

/// The unstable items a probe passed over for a stable one of another trait: one for a method
/// call as for a type-relative path, rustc's `Pick::unstable_candidates` for both.
#[test]
fn a_method_call_keeps_the_unstable_method_its_probe_passed_over() {
    let db = fixture::db(UNSTABLE_METHOD).into_db();
    crate::attach_db(&db, || {
        for name in ["call", "path"] {
            let (_, result) = infer(&db, name);
            let found: Vec<_> = result // alloc: a test's list
                .unstable_candidate_nodes()
                .flat_map(|(_, candidates)| candidates.iter().map(|(_, feature)| feature.as_str()))
                .collect();
            let want = if name == "call" { "ipu_flatten" } else { "assoc_const_ipu_iter" };
            assert_eq!(found, [want], "`{name}`");
        }
    });
}
