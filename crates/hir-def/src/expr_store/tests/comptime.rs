//! `#![feature(blaze_comptime)]` lowering (`docs/v2/plan/design/comptime.md`): `for const` to
//! `Expr::ComptimeFor`, `x.[f]` to `Expr::ReflectedField`, a bounded `for<F: ..>` binder to the
//! generics' binder parameters, `const static { .. }` to an inline const; and without the feature, each one the `ComptimeWithoutFeature`
//! error and nothing else.

use hir_expand::fixture;

use super::super::*;
use crate::{
    DefWithBodyId, FunctionId, ModuleDefId,
    hir::generics::{BinderParams, WherePredicate},
    nameres::crate_def_map,
    signatures::FunctionSignature,
    test_db::{TestDB, TestDBExt},
};

const FEATURE: &str = "#![feature(blaze_comptime)]\n";

/// The test crate's first function.
fn first_function(#[rust_analyzer::rust_fixture] ra_fixture: &str) -> (TestDB, FunctionId) {
    let db = fixture::db(ra_fixture).into_db();
    let def_map = crate_def_map(&db, db.fetch_test_crate());
    let function = def_map
        .modules(&db)
        .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
        .find_map(|decl| match decl {
            ModuleDefId::FunctionId(it) => Some(it),
            _ => None,
        })
        .expect("a function");
    (db, function)
}

fn forms(diagnostics: &[ExpressionStoreDiagnostics]) -> Vec<ComptimeForm> {
    diagnostics
        .iter()
        .filter_map(|it| match it {
            ExpressionStoreDiagnostics::ComptimeWithoutFeature { form, .. } => Some(*form),
            _ => None,
        })
        .collect()
}

/// The first function of `text`, with or without the feature: the forms its body's lowering
/// refused. Its body round-trips through the store's format.
fn lower_body(text: &str, feature: bool) -> (Vec<ComptimeForm>, TestDB, DefWithBodyId) {
    let fixture = format!("{}{text}", if feature { FEATURE } else { "" });
    let (db, function) = first_function(&fixture);
    let def = DefWithBodyId::from(function);
    let (body, source_map) = Body::with_source_map(&db, def);
    let forms = forms(source_map.diagnostics());
    crate::expr_store::persist::assert_round_trip(body, source_map);
    (forms, db, def)
}

#[test]
fn for_const_lowers_to_a_comptime_for() {
    let text = "fn f(x: ()) { 'l: for const f in x { let _ = f; break 'l; } }";
    let (forms, db, def) = lower_body(text, true);
    assert_eq!(forms, []);
    let store = Body::of(&db, def);
    let (pat, iterable, body, label) = store
        .exprs()
        .find_map(|(_, it)| match it {
            Expr::ComptimeFor { pat, iterable, body, label } => Some((pat, iterable, body, label)),
            _ => None,
        })
        .expect("a `ComptimeFor`");
    assert!(matches!(store.pat(pat), Pat::Bind { .. }), "the loop's pattern is `f`");
    assert!(matches!(store.expr(iterable), Expr::Path(_)), "the iterable is `x`");
    assert!(matches!(store.expr(body), Expr::Block { .. }), "the body is the block");
    assert!(label.is_some(), "`'l` is the loop's label");
    // Not desugared: no `IntoIterator::into_iter` call, no `loop`.
    assert!(!store.exprs().any(|(_, it)| matches!(it, Expr::Loop { .. } | Expr::Call { .. })));
    // `break 'l` targets the comptime loop's label.
    assert!(
        store
            .exprs()
            .any(|(_, it)| matches!(it, Expr::Break { label: Some(l), .. } if Some(l) == label))
    );

    let (forms, db, def) = lower_body(text, false);
    assert_eq!(forms, [ComptimeForm::ForConst]);
    let store = Body::of(&db, def);
    assert!(!store.exprs().any(|(_, it)| matches!(it, Expr::ComptimeFor { .. })));
}

#[test]
fn a_reflected_field_lowers_to_a_reflected_field() {
    let text = "fn f(x: (), g: ()) { let _ = x.[g]; }";
    let (forms, db, def) = lower_body(text, true);
    assert_eq!(forms, []);
    let store = Body::of(&db, def);
    let (base, field) = store
        .exprs()
        .find_map(|(_, it)| match it {
            Expr::ReflectedField { base, field } => Some((base, field)),
            _ => None,
        })
        .expect("a `ReflectedField`");
    assert_ne!(base, field);
    assert!(!store.exprs().any(|(_, it)| matches!(it, Expr::Index { .. })), "not an index");

    let (forms, db, def) = lower_body(text, false);
    assert_eq!(forms, [ComptimeForm::ReflectedField]);
    let store = Body::of(&db, def);
    assert!(
        !store
            .exprs()
            .any(|(_, it)| matches!(it, Expr::ReflectedField { .. } | Expr::Index { .. }))
    );
}

/// `const static { .. }` is an inline const (`Expr::Const`) of the block, as `const { .. }` is:
/// what it is is read off its syntax later. Without the feature it is `Missing`, with the error.
#[test]
fn const_static_lowers_to_an_inline_const() {
    let text = "fn f() -> u8 { let x = const static { 1 }; x }";
    let (forms, db, def) = lower_body(text, true);
    assert_eq!(forms, []);
    let store = Body::of(&db, def);
    let inner = store
        .exprs()
        .find_map(|(_, it)| match it {
            Expr::Const(inner) => Some(inner),
            _ => None,
        })
        .expect("an `Expr::Const`");
    assert!(matches!(store.expr(inner), Expr::Block { .. }), "the inline const's block");

    let (forms, db, def) = lower_body(text, false);
    assert_eq!(forms, [ComptimeForm::Block]);
    let store = Body::of(&db, def);
    assert!(!store.exprs().any(|(_, it)| matches!(it, Expr::Const(_))));
}

#[test]
fn a_bounded_binder_parameter_is_a_binder_param_of_the_generics() {
    let text = "trait Field<T> {} trait Hash {} trait Eq {}\n\
                fn f<T>() where for<'a, F: Field<T>> F: Hash + Eq {}";
    let (db, function) = first_function(&format!("{FEATURE}{text}"));
    let (signature, source_map) = FunctionSignature::with_source_map(&db, function);
    assert_eq!(forms(source_map.diagnostics()), []);
    let params = &signature.generic_params;
    let binders: Vec<_> = params
        .where_predicates()
        .iter()
        .filter_map(|it| match *it {
            WherePredicate::TypeBound { binder, lifetimes, .. } => {
                Some((binder, lifetimes.is_some()))
            }
            WherePredicate::Lifetime { .. } => None,
        })
        .collect();
    // `Hash` and `Eq` split the predicate in two, sharing the binder and its `'a`.
    assert_eq!(binders, [(BinderParams { start: 0, len: 1 }, true); 2]);
    let (_, param) = params.binder_params(binders[0].0).next().expect("`F`");
    assert_eq!(param.name.as_str(), "F");
    assert_eq!(param.bounds.len(), 1, "`F: Field<T>`");
    assert_eq!(params.len(), 1, "`F` is no generic parameter of `f`");
    crate::expr_store::persist::assert_store_round_trip(&signature.store, source_map);

    let (db, function) = first_function(text);
    let (signature, source_map) = FunctionSignature::with_source_map(&db, function);
    assert_eq!(forms(source_map.diagnostics()), [ComptimeForm::BinderBound]);
    assert_eq!(signature.generic_params.iter_binder_params().count(), 0);
    assert!(signature.generic_params.where_predicates().iter().all(|it| match it {
        WherePredicate::TypeBound { binder, .. } => binder.is_empty(),
        WherePredicate::Lifetime { .. } => true,
    }));
}

/// Without the feature, an unbounded binder type parameter (`non_lifetime_binders`) is left out
/// as before, with no error.
#[test]
fn an_unbounded_binder_parameter_without_the_feature_is_as_before() {
    let (db, function) = first_function("trait Hash {}\nfn f<T>() where for<F> T: Hash {}");
    let (signature, source_map) = FunctionSignature::with_source_map(&db, function);
    assert_eq!(forms(source_map.diagnostics()), []);
    assert_eq!(signature.generic_params.iter_binder_params().count(), 0);
}
