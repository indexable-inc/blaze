//! `for const` and `base.[f]` (`#![feature(blaze_comptime)]`, docs/v2/plan/design/comptime.md,
//! "Type checking once"): a loop's body is checked once, against what its binder's bounds and
//! the where clauses over its container say, never per field; without the feature nothing of it
//! reaches inference.

use hir_def::{
    AdtId, AssocItemId, DefWithBodyId, FunctionId, GenericDefId, ModuleDefId,
    expr_store::Body,
    hir::Expr,
    nameres::crate_def_map,
    signatures::{FunctionSignature, StructSignature},
};
use hir_expand::fixture;
use rustc_type_ir::{
    BoundVariableKind, TypeVisitableExt,
    inherent::{GenericArgs as _, IntoKind},
};

use crate::{
    InferBodyId, InferenceDiagnostic, InferenceResult, db::HirDatabase, lower::GenericPredicates,
    next_solver::TyKind,
};

/// A `core` with the reflection lang items (`library/core/src/reflect.rs` reduced to what
/// inference reads).
const REFLECT: &str = r#"
#[lang = "field"]
pub unsafe trait FieldOf: Copy {
    #[lang = "field_base"]
    type Base: ?Sized;
    #[lang = "field_type"]
    type Type: ?Sized;
}
#[lang = "blaze_field"]
pub unsafe trait Field<B: ?Sized>: FieldOf {}
#[lang = "blaze_variant"]
pub unsafe trait Variant<B>: Copy {}
#[lang = "blaze_fields"]
pub struct Fields<T: ?Sized, P = ()>(*const T, P);
#[lang = "blaze_variants"]
pub struct Variants<T: ?Sized>(*const T);
pub fn fields_of<T: ?Sized>() -> Fields<T> { loop {} }

pub trait Show {}
fn show<X: Show + ?Sized>(_: &X) {}
"#;

const BODIES: &str = r#"
fn bounded<T>(t: &T) where for<F: Field<T>> F::Type: Show {
    for const f in fields_of::<T>() {
        show(&t.[f]);
    }
}
fn unbounded<T>(t: &T) {
    for const f in fields_of::<T>() {
        show(&t.[f]);
    }
}
"#;

fn function(db: &base_db::Db, name: &str) -> FunctionId {
    crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("the fixture's `{name}`"))
}

/// The method `name` of the fixture's inherent impl on the struct `owner` (`impl A`,
/// `impl Wrap<u8>`).
fn method(db: &base_db::Db, owner: &str, name: &str) -> FunctionId {
    crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).impls().collect::<Vec<_>>())
        .filter(|&imp| match db.impl_self_ty(imp).skip_binder().as_adt() {
            Some((AdtId::StructId(it), _)) => StructSignature::of(db, it).name.as_str() == owner,
            _ => false,
        })
        .flat_map(|imp| imp.impl_items(db).items.iter().map(|&(_, item)| item).collect::<Vec<_>>())
        .find_map(|item| match item {
            AssocItemId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("the fixture's method `{owner}::{name}`"))
}

/// The methods `func`'s method calls resolve to, by name, in body order; `None` for a call
/// left unresolved.
fn method_calls(db: &base_db::Db, func: FunctionId) -> Vec<(String, Option<FunctionId>)> {
    let def = DefWithBodyId::FunctionId(func);
    let result = infer(db, func);
    Body::of(db, def)
        .exprs()
        .filter_map(|(id, expr)| match expr {
            Expr::MethodCall { method_name, .. } => Some((
                method_name.as_str().to_owned(),
                result.method_resolution(id).map(|(it, _)| it),
            )),
            _ => None,
        })
        .collect()
}

/// `func`'s environment holds no `{type error}`: lowering normalized every field binder
/// (an error type unifies with everything, so a body checked in such an environment hides it).
fn assert_env_has_no_error(db: &base_db::Db, func: FunctionId) {
    let env = db.trait_environment(GenericDefId::FunctionId(func));
    let errors =
        env.clauses().iter().filter(|clause| clause.references_error()).collect::<Vec<_>>();
    assert!(errors.is_empty(), "the environment of {func:?} has error clauses: {errors:?}");
}

fn infer<'db>(db: &'db base_db::Db, func: FunctionId) -> &'db InferenceResult<'db> {
    InferenceResult::of(db, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(func)))
}

#[test]
fn a_for_const_body_is_checked_once_against_its_binders_bounds() {
    let fixture = format!(
        "//- minicore: sized, copy, clone\n#![feature(blaze_comptime, lang_items)]\n{REFLECT}{BODIES}"
    );
    let db = fixture::db(&fixture).into_db();
    blaze_query::db::attach(&db, || {
        let bounded = function(&db, "bounded");
        let lowered =
            GenericPredicates::query_with_diagnostics(&db, GenericDefId::FunctionId(bounded));
        assert!(lowered.diagnostics().is_empty(), "{:?}", lowered.diagnostics());
        let binder_clauses = lowered
            .value
            .all_predicates()
            .iter_identity()
            .filter(|it| {
                it.skip_norm_wip()
                    .kind()
                    .bound_vars()
                    .iter()
                    .any(|var| matches!(var, BoundVariableKind::Reflected(_)))
            })
            .count();
        assert_eq!(
            binder_clauses, 1,
            "`for<F: Field<T>> F::Type: Show` is one field binder clause"
        );

        assert_env_has_no_error(&db, bounded);
        let result = infer(&db, bounded);
        assert!(result.diagnostics().is_empty(), "{:?}", result.diagnostics());
        assert!(!result.has_type_mismatches());
        assert!(!result.has_errors, "`bounded` was inferred with error types");
        let [(_, comptime_loop)] = result.comptime_loops().collect::<Vec<_>>()[..] else {
            panic!("one checked loop, not {:?}", result.comptime_loops().collect::<Vec<_>>());
        };
        assert!(matches!(comptime_loop.binder.as_ref().kind(), TyKind::Placeholder(_)));
        assert!(result.comptime_max_universe() >= comptime_loop.universe());

        // The same body with no bound on the fields' types: `show` is refused once, for `F`,
        // not once per field.
        let result = infer(&db, function(&db, "unbounded"));
        assert_eq!(result.comptime_loops().len(), 1);
        assert!(
            !result.diagnostics().is_empty()
                && result
                    .diagnostics()
                    .iter()
                    .all(|it| !matches!(it, InferenceDiagnostic::ComptimeNotYetSupported { .. })),
            "{:?}",
            result.diagnostics()
        );
    });
}

#[test]
fn without_the_feature_inference_sees_no_comptime_loop() {
    let fixture = format!("//- minicore: sized, copy, clone\n{REFLECT}{BODIES}");
    let db = fixture::db(&fixture).into_db();
    blaze_query::db::attach(&db, || {
        for name in ["bounded", "unbounded"] {
            let func = function(&db, name);
            let result = infer(&db, func);
            assert_eq!(result.comptime_loops().len(), 0);
            assert!(
                result
                    .diagnostics()
                    .iter()
                    .all(|it| !matches!(it, InferenceDiagnostic::ComptimeNotYetSupported { .. }))
            );
            let env = db.trait_environment(GenericDefId::FunctionId(func));
            assert_eq!(result.comptime_param_env(env), env);
        }
    });
}

/// The method probe works out a receiver's autoderef steps in the body's own inference context,
/// in a probe: a binder in the receiver must stay the caller's own placeholder there, not be
/// made anew in the current universe. Inside the inner loop that is `g`'s universe, and the
/// outer `f`, made anew, would be `g`: its field would deref by `g`'s bound to `B`, which has no
/// `a`.
#[test]
fn a_method_on_an_outer_binders_field_derefs_by_its_own_bound_in_an_inner_loop() {
    let fixture = format!(
        "//- minicore: sized, copy, clone, deref\n#![feature(blaze_comptime, lang_items)]\n{REFLECT}{}",
        r#"
pub struct A;
impl A { pub fn a(&self) {} }
pub struct B;
impl B { pub fn b(&self) {} }
fn nested<T, U>(t: &T, u: &U)
where
    for<F: Field<T>> F::Type: core::ops::Deref<Target = A>,
    for<G: Field<U>> G::Type: core::ops::Deref<Target = B>,
{
    for const f in fields_of::<T>() {
        for const g in fields_of::<U>() {
            t.[f].a();
            u.[g].b();
        }
    }
}
"#
    );
    let db = fixture::db(&fixture).into_db();
    blaze_query::db::attach(&db, || {
        let nested = function(&db, "nested");
        assert_env_has_no_error(&db, nested);
        let result = infer(&db, nested);
        assert_eq!(result.comptime_loops().len(), 2);
        assert!(result.diagnostics().is_empty(), "{:?}", result.diagnostics());
        assert!(!result.has_type_mismatches());
        assert!(!result.has_errors, "`nested` was inferred with error types");
        assert_eq!(
            method_calls(&db, nested),
            [
                ("a".to_owned(), Some(method(&db, "A", "a"))),
                ("b".to_owned(), Some(method(&db, "B", "b")))
            ]
        );
    });
}

/// A receiver `Wrap<?x>` whose deref step is found through an environment bound on the loop's
/// binder (`Wrap<!f>: Deref<Target = Inner<!f>>`, from
/// `for<F: Field<T>> Wrap<F>: Deref<Target = Inner<F>>`): the second step commits `?x := !f`
/// and is `Inner<!f>`, so `Inner<u8>::a` does not apply and `HasA::a` (by the bound
/// `Inner<!f>: HasA`) is picked. The steps' response names `!f` where the probe's input has
/// `?x`; mapped back to `?x`, the step would be `Inner<?x>`, `?x` left free for
/// `Inner<u8>::a` to take.
#[test]
fn a_receiver_inferred_through_a_bound_on_the_binder_derefs_by_that_bound() {
    let fixture = format!(
        "//- minicore: sized, copy, clone, deref\n#![feature(blaze_comptime, lang_items)]\n{REFLECT}{}",
        r#"
pub struct Inner<X>(*const X);
impl Inner<u8> { pub fn a(&self) {} }
pub trait HasA { fn a(&self); }
pub struct Wrap<X>(*const X);
fn wrap<X>() -> Wrap<X> { loop {} }
fn through<T>(t: &T)
where
    for<F: Field<T>> Wrap<F>: core::ops::Deref<Target = Inner<F>>,
    for<F: Field<T>> Inner<F>: HasA,
{
    for const f in fields_of::<T>() {
        let w = wrap();
        w.a();
    }
}
"#
    );
    let db = fixture::db(&fixture).into_db();
    blaze_query::db::attach(&db, || {
        let through = function(&db, "through");
        assert_env_has_no_error(&db, through);
        let result = infer(&db, through);
        let calls = method_calls(&db, through);
        let [(_, Some(picked))] = calls[..] else { panic!("one resolved call, not {calls:?}") };
        assert_ne!(picked, method(&db, "Inner", "a"), "`w.a()` picked `Inner<u8>::a`");
        assert!(result.diagnostics().is_empty(), "{:?}", result.diagnostics());
        assert!(!result.has_type_mismatches());
        assert!(!result.has_errors, "`through` was inferred with error types");
        let [(_, comptime_loop)] = result.comptime_loops().collect::<Vec<_>>()[..] else {
            panic!("one checked loop");
        };
        // `wrap()`: the call whose type has one argument (`fields_of::<T>()`'s `Fields` has two).
        let wrapped = Body::of(&db, DefWithBodyId::FunctionId(through))
            .exprs()
            .filter(|(_, expr)| matches!(expr, Expr::Call { .. }))
            .map(|(id, _)| result.expr_ty(id))
            .find(|ty| matches!(ty.kind(), TyKind::Adt(_, args) if args.len() == 1))
            .expect("the `wrap()` call");
        let TyKind::Adt(_, args) = wrapped.kind() else { unreachable!() };
        assert_eq!(args.type_at(0), comptime_loop.binder.as_ref(), "`wrap()` is {wrapped:?}");
    });
}

/// A binder whose second variable ranges over the fields of the first's type
/// (`for<F: Field<T>, G: Field<F::Type>>`) is entered in order, as the solver enters it: `G`'s
/// container is normalized with `F` a member, and `G` gets the environment's binders over it,
/// so `<G::Type as Tr>::Assoc` normalizes by the bound `G::Type: Tr<Assoc = A>`.
#[test]
fn a_dependent_field_binder_normalizes_with_its_earlier_variables_entered() {
    let fixture = format!(
        "//- minicore: sized, copy, clone\n#![feature(blaze_comptime, lang_items)]\n{REFLECT}{}",
        r#"
pub struct A;
impl Show for A {}
pub trait Tr { type Assoc; }
fn dependent<T>(t: &T)
where
    for<F: Field<T>, G: Field<F::Type>> G::Type: Tr<Assoc = A>,
    for<F: Field<T>, G: Field<F::Type>> <G::Type as Tr>::Assoc: Show,
{
}
"#
    );
    let db = fixture::db(&fixture).into_db();
    blaze_query::db::attach(&db, || {
        let dependent = function(&db, "dependent");
        let lowered =
            GenericPredicates::query_with_diagnostics(&db, GenericDefId::FunctionId(dependent));
        assert!(lowered.diagnostics().is_empty(), "{:?}", lowered.diagnostics());
        assert_env_has_no_error(&db, dependent);
    });
}
