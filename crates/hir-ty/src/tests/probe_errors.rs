//! The errors rustc reports from autoderef, the method probe, reference patterns and `Fn*`
//! calls are errors here too, never a failed probe or an error type with no diagnostic
//! (queue 01168): E0055, E0282 of an unknown receiver, E0308 of `&mut` matching an inherited
//! `&`, and E0059.

use hir_def::{DefWithBodyId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;

use crate::{InferBodyId, InferenceDiagnostic, InferenceResult, solver_errors::TypeAnnotationNeeded};

/// The inference diagnostics of the function `f` of `fixture`.
fn diagnostics(fixture: &str) -> Vec<InferenceDiagnostic> {
    let db = fixture::db(fixture).into_db();
    crate::next_solver::attach_db(&db, || {
        let function = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::FunctionId(it) if FunctionSignature::of(&db, it).name.as_str() == "f" => Some(it),
                _ => None,
            })
            .expect("the fixture's `f`");
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)));
        infer.diagnostics().to_vec()
    })
}

const SELF_DEREF: &str = r#"
//- minicore: deref
use core::ops::Deref;
struct S;
impl Deref for S {
    type Target = S;
    fn deref(&self) -> &S { self }
}
"#;

#[test]
fn a_method_on_a_type_that_derefs_to_itself_is_e0055() {
    let found = diagnostics(&format!("{SELF_DEREF}fn f() {{ S.missing(); }}"));
    let limits: Vec<u32> = found
        .iter()
        .filter_map(|it| match it {
            InferenceDiagnostic::AutoderefRecursionLimit { suggested_limit, .. } => Some(*suggested_limit),
            _ => None,
        })
        .collect();
    assert_eq!(limits, [256], "one E0055, suggesting twice the default limit: {found:?}");
}

#[test]
fn a_field_of_a_type_that_derefs_to_itself_is_e0055() {
    let found = diagnostics(&format!("{SELF_DEREF}fn f() {{ let _ = S.missing; }}"));
    assert!(
        found.iter().any(|it| matches!(it, InferenceDiagnostic::AutoderefRecursionLimit { .. })),
        "{found:?}",
    );
}

#[test]
fn a_receiver_deeper_than_twenty_derefs_resolves() {
    let refs = "&".repeat(30);
    let found = diagnostics(&format!(
        "//- minicore: sized\nstruct T;\nimpl T {{ fn m(&self) {{}} }}\nfn f() {{ let t = {refs}T; t.m(); }}"
    ));
    assert!(found.is_empty(), "rustc's limit is the crate's (128), not 20: {found:?}");
}

const UNKNOWN: &str = r#"
//- minicore: sized
fn reference<T>() -> &'static T { loop {} }
fn pointer<T>() -> *const T { loop {} }
"#;

#[test]
fn a_method_on_a_reference_to_an_unknown_type_is_e0282() {
    let found = diagnostics(&format!("{UNKNOWN}fn f() {{ let r = reference(); r.m(); let _: &u8 = r; }}"));
    assert!(
        found.iter().any(|it| matches!(
            it,
            InferenceDiagnostic::TypeMustBeKnown { kind: TypeAnnotationNeeded::E0282, .. }
        )),
        "the receiver is known only after the call: {found:?}",
    );
}

#[test]
fn a_method_on_a_raw_pointer_to_an_unknown_type_is_e0282_from_2018() {
    let found = diagnostics(&format!("{UNKNOWN}fn f() {{ let p = pointer(); p.m(); }}"));
    assert!(
        found.iter().any(|it| matches!(it, InferenceDiagnostic::MethodOnUnknownRawPointee { .. })),
        "{found:?}",
    );
    assert!(
        !found.iter().any(|it| matches!(it, InferenceDiagnostic::TyvarBehindRawPointer { .. })),
        "the lint is 2015's: {found:?}",
    );
}

#[test]
fn a_method_on_a_raw_pointer_to_an_unknown_type_only_lints_in_2015() {
    let fixture = format!(
        "//- minicore: sized\n//- /main.rs edition:2015\n{}fn f() {{ let p = pointer(); p.m(); }}",
        UNKNOWN.trim_start_matches("\n//- minicore: sized\n"),
    );
    let found = diagnostics(&fixture);
    assert!(
        !found.iter().any(|it| matches!(it, InferenceDiagnostic::MethodOnUnknownRawPointee { .. })),
        "{found:?}",
    );
    let lints = found
        .iter()
        .filter(|it| matches!(it, InferenceDiagnostic::TyvarBehindRawPointer { .. }))
        .count();
    assert_eq!(lints, 1, "rustc's `tyvar_behind_raw_pointer`, once: {found:?}");
}

#[test]
fn mut_matching_an_inherited_shared_reference_is_e0308() {
    let found = diagnostics(
        "//- minicore: sized\n//- /main.rs edition:2024\n#![feature(ref_pat_eat_one_layer_2024)]\nfn f() { let [&mut x] = &[0u8]; }",
    );
    assert!(
        found.iter().any(|it| matches!(it, InferenceDiagnostic::InheritedRefMutabilityMismatch { .. })),
        "{found:?}",
    );
}

#[test]
fn a_call_through_fn_of_a_non_tuple_is_e0059() {
    let found = diagnostics(
        "//- minicore: fn\n#![feature(unboxed_closures)]\nfn f<F: Fn<u8>>(g: F) { g(1u8); }",
    );
    assert!(found.iter().any(|it| matches!(it, InferenceDiagnostic::FnTraitArgsNotTuple { .. })), "{found:?}");
}
