//! A const argument in a body that is not a constant is an error at the argument, as rustc's
//! resolver and const checking give it, never a fresh inference variable the body goes on with
//! (queue 01168).

use hir_def::{
    AdtId, DefWithBodyId, ModuleDefId, VariantId,
    nameres::crate_def_map,
    signatures::{FunctionSignature, StructSignature},
};
use hir_expand::fixture;

use crate::{
    InferBodyId, InferenceDiagnostic, InferenceResult, Span, TyLoweringDiagnostic,
    consteval::CreateConstError, db::HirDatabase,
};

const FIXTURE: &str = r#"
//- minicore: sized
struct T;
impl T {
    fn m<const N: usize>(&self) {}
}
static S: usize = 1;
fn local() {
    let n = 1usize;
    T.m::<n>();
}
fn statik() {
    T.m::<{ S }>();
}
fn mismatched() {
    T.m::<true>();
}
fn fine() {
    T.m::<3>();
}
"#;

/// The inference diagnostics of the fixture's function `name`.
fn diagnostics(name: &str) -> Vec<InferenceDiagnostic> {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let function = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::FunctionId(it) if FunctionSignature::of(&db, it).name.as_str() == name => Some(it),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the fixture's `{name}`"));
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)));
        infer.diagnostics().to_vec()
    })
}

fn invalid_const_args(diagnostics: &[InferenceDiagnostic]) -> Vec<(Span, CreateConstError)> {
    diagnostics
        .iter()
        .filter_map(|it| match it {
            InferenceDiagnostic::InvalidConstArg { site, reason } => Some((*site, reason.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_local_as_a_const_arg_is_e0435_at_its_type_ref() {
    let found = invalid_const_args(&diagnostics("local"));
    assert!(
        matches!(found.as_slice(), [(Span::TypeRefId(_), CreateConstError::LocalBinding)]),
        "`m::<n>` names a local: {found:?}",
    );
}

#[test]
fn a_static_as_a_const_arg_is_e0013_at_its_expression() {
    let found = invalid_const_args(&diagnostics("statik"));
    assert!(
        matches!(found.as_slice(), [(Span::ExprId(_), CreateConstError::Static)]),
        "`m::<{{ S }}>` reads a static: {found:?}",
    );
}

#[test]
fn a_literal_of_another_type_is_a_type_mismatch() {
    let diagnostics = diagnostics("mismatched");
    assert!(invalid_const_args(&diagnostics).is_empty(), "{diagnostics:?}");
    assert!(
        diagnostics.iter().any(|it| matches!(it, InferenceDiagnostic::TypeMismatch { .. })),
        "`m::<true>` for a `usize` parameter is E0308: {diagnostics:?}",
    );
}

#[test]
fn a_constant_const_arg_has_no_error() {
    let diagnostics = diagnostics("fine");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

/// In a type, a static is E0013 at its path, and a literal of another type is an anon const,
/// whose own type check gives E0308 as rustc's.
#[test]
fn a_field_type_reports_its_invalid_const_args() {
    let db = fixture::db(
        r#"
//- minicore: sized
struct W<const N: usize>;
static S: usize = 1;
struct V {
    a: W<S>,
    b: W<true>,
    c: W<3>,
}
"#,
    )
    .into_db();
    crate::next_solver::attach_db(&db, || {
        let v = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::AdtId(AdtId::StructId(it)) if StructSignature::of(&db, it).name.as_str() == "V" => {
                    Some(it)
                }
                _ => None,
            })
            .expect("the fixture's `V`");
        let lowered = db.field_types_with_diagnostics(VariantId::StructId(v));
        assert!(
            matches!(
                lowered.diagnostics(),
                [TyLoweringDiagnostic::InvalidConstArg { site: Span::TypeRefId(_), reason: CreateConstError::Static }]
            ),
            "only `W<S>` is an error of lowering: {:?}",
            lowered.diagnostics(),
        );
        assert_eq!(lowered.defined_anon_consts().len(), 1, "`W<true>` is an anon const");
    });
}
