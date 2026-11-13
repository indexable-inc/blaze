//! The errors rustc reports from autoderef, the method probe, reference patterns and `Fn*`
//! calls are errors here too, never a failed probe or an error type with no diagnostic:
//! E0055, E0282 of an unknown receiver, E0308 of `&mut` matching an inherited
//! `&`, and E0059.

use hir_def::{DefWithBodyId, ModuleDefId, nameres::crate_def_map, signatures::FunctionSignature};
use hir_expand::fixture;

use crate::{
    InferBodyId, InferenceDiagnostic, InferenceResult, solver_errors::TypeAnnotationNeeded,
};

/// The inference diagnostics of the function `f` of `fixture`.
fn diagnostics(fixture: &str) -> Vec<InferenceDiagnostic> {
    let db = fixture::db(fixture).into_db();
    blaze_query::db::attach(&db, || {
        let function = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::FunctionId(it)
                    if FunctionSignature::of(&db, it).name.as_str() == "f" =>
                {
                    Some(it)
                }
                _ => None,
            })
            .expect("the fixture's `f`");
        let infer = InferenceResult::of(
            &db,
            InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)),
        );
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
            InferenceDiagnostic::AutoderefRecursionLimit { suggested_limit, .. } => {
                Some(*suggested_limit)
            }
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
fn a_type_with_an_error_derefs_to_nothing_and_is_no_e0055() {
    // As blaze-codegen-code's `pinned.version` once was, with `pin::Pin` unresolved: the
    // unresolved path is the error, and a field or method of the error type is no E0055.
    let found = diagnostics(
        "//- minicore: deref, sized\n\
         mod pin {}\n\
         fn pinned() -> (pin::Pin, u8) { loop {} }\n\
         fn f() { let (p, _) = pinned(); let _ = p.version; p.refusal(); let _ = (&&p).commit; }",
    );
    assert!(
        !found.iter().any(|it| matches!(it, InferenceDiagnostic::AutoderefRecursionLimit { .. })),
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
    let found =
        diagnostics(&format!("{UNKNOWN}fn f() {{ let r = reference(); r.m(); let _: &u8 = r; }}"));
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

/// Rust 1.99 (rust-lang/rust#156047): the receiver's type is a variable a coercion of `!`
/// made, so the probe sets it to `!` and finds `!`'s method, with the lint; with no such
/// coercion the same call is E0282 (the control: `unknown()` in place of `loop {}`).
#[test]
fn a_method_on_a_diverging_variable_is_nevers_and_linted() {
    let source = |init: &str| {
        format!(
            "//- minicore: sized\ntrait Tr {{ fn m(&self) -> u8 {{ 1 }} }}\nimpl Tr for ! {{}}\n\
             fn unknown<T>() -> T {{ loop {{}} }}\nfn f() {{ let x = {init}; let _: u8 = x.m(); }}"
        )
    };
    let lint = |it: &InferenceDiagnostic| {
        matches!(it, InferenceDiagnostic::MethodCallOnDivergingInferVar { .. })
    };
    let found = diagnostics(&source("loop {}"));
    assert_eq!(found.iter().filter(|it| lint(it)).count(), 1, "the lint, once: {found:?}");
    assert_eq!(found.len(), 1, "and nothing else: the method is `!`'s: {found:?}");

    let found = diagnostics(&source("unknown()"));
    assert!(!found.iter().any(lint), "no coercion of `!` made this variable: {found:?}");
    assert!(
        found.iter().any(|it| matches!(
            it,
            InferenceDiagnostic::TypeMustBeKnown { kind: TypeAnnotationNeeded::E0282, .. }
        )),
        "{found:?}",
    );
}

#[test]
fn mut_matching_an_inherited_shared_reference_is_e0308() {
    let found = diagnostics(
        "//- minicore: sized\n//- /main.rs edition:2024\n#![feature(ref_pat_eat_one_layer_2024)]\nfn f() { let [&mut x] = &[0u8]; }",
    );
    assert!(
        found
            .iter()
            .any(|it| matches!(it, InferenceDiagnostic::InheritedRefMutabilityMismatch { .. })),
        "{found:?}",
    );
}

#[test]
fn a_call_through_fn_of_a_non_tuple_is_e0059() {
    let found = diagnostics(
        "//- minicore: fn\n#![feature(unboxed_closures)]\nfn f<F: Fn<u8>>(g: F) { g(1u8); }",
    );
    assert!(
        found.iter().any(|it| matches!(it, InferenceDiagnostic::FnTraitArgsNotTuple { .. })),
        "{found:?}"
    );
}

/// The methods `f` of `fixture` calls that are not found, each with the numeric variable its
/// receiver was where the method was looked up.
fn unresolved_methods(fixture: &str) -> Vec<(String, Option<crate::NumericVar>)> {
    diagnostics(fixture)
        .iter()
        .filter_map(|it| match it {
            InferenceDiagnostic::UnresolvedMethodCall { name, numeric_var, .. } => {
                Some((name.as_str().to_owned(), *numeric_var))
            }
            _ => None,
        })
        .collect()
}

/// A method called on an integer or a float whose type is not decided yet is an error where
/// it is called, as in rustc (E0689), also when a later use decides the type and the type has
/// the method: a method call does not wait for later constraints. The error records that the
/// receiver was the variable, so no message names the type it became.
#[test]
fn a_method_on_a_number_of_undecided_type_is_an_error_whatever_decides_it_later() {
    use crate::NumericVar::{Float, Integer};
    let core = "//- minicore: sized, num_methods\n";
    let found = unresolved_methods(&format!(
        "{core}fn f() {{ let mut pos = 0; let start = pos; let end = start.saturating_add(4); \
         pos = end; let _: usize = pos; }}"
    ));
    assert_eq!(found, [("saturating_add".to_owned(), Some(Integer))]);
    let found = unresolved_methods(&format!(
        "{core}fn f() {{ let ratio = 2.0; ratio.powf(2.0); let _: f32 = ratio; }}"
    ));
    assert_eq!(found, [("powf".to_owned(), Some(Float))]);
    let found = unresolved_methods(&format!("{core}fn f() {{ 1.saturating_add(2); }}"));
    assert_eq!(found, [("saturating_add".to_owned(), Some(Integer))]);
}

/// The control: once the number's type is decided where the method is called, the method is
/// found, and one the type lacks is an ordinary unresolved method of that type.
#[test]
fn a_method_on_a_number_of_decided_type_is_looked_up_on_it() {
    let core = "//- minicore: sized, num_methods\n";
    let found = unresolved_methods(&format!(
        "{core}fn f() {{ let pos: usize = 0; pos.saturating_add(4); let x = 2.0f32; x.powf(2.0); \
         let y = 1; let _: u8 = y; y.saturating_add(1); }}"
    ));
    assert!(found.is_empty(), "{found:?}");
    let found = unresolved_methods(&format!("{core}fn f() {{ let pos: usize = 0; pos.nope(); }}"));
    assert_eq!(found, [("nope".to_owned(), None)]);
}
