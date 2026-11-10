//! A const argument in a body that is not a constant is an error at the argument, as rustc's
//! resolver and const checking give it, never a fresh inference variable the body goes on with.

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
fn local() {
    let n = 1usize;
    T.m::<n>();
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
    blaze_query::db::attach(&db, || {
        let function = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::FunctionId(it)
                    if FunctionSignature::of(&db, it).name.as_str() == name =>
                {
                    Some(it)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("the fixture's `{name}`"));
        let infer = InferenceResult::of(
            &db,
            InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(function)),
        );
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

// A constant may read a static (`const_refs_to_static`): `m::<{ S }>()` is tested in
// hir-ty-mir (`consteval::a_static_as_a_const_arg_is_an_anon_const_that_reads_it`), since type
// checking the call normalizes `{ S }`, which evaluates it, and this binary builds no MIR.

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

/// In a type, an array length that names a static is an anon const that reads it (no E0013, as
/// rustc since 1.83), and so is a literal of another type, whose own type check gives E0308 as
/// rustc's. A bare path to a static as a generic argument, which rustc also makes an anon
/// const, has no expression for blaze to make one of yet, and says so.
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
    d: [u8; S],
}
"#,
    )
    .into_db();
    blaze_query::db::attach(&db, || {
        let v = crate_def_map(&db, fixture::test_crate(&db))
            .modules(&db)
            .flat_map(|(_, module)| module.scope(&db).declarations().collect::<Vec<_>>())
            .find_map(|def| match def {
                ModuleDefId::AdtId(AdtId::StructId(it))
                    if StructSignature::of(&db, it).name.as_str() == "V" =>
                {
                    Some(it)
                }
                _ => None,
            })
            .expect("the fixture's `V`");
        let lowered = db.field_types_with_diagnostics(VariantId::StructId(v));
        assert!(
            matches!(
                lowered.diagnostics(),
                [TyLoweringDiagnostic::InvalidConstArg {
                    site: Span::TypeRefId(_),
                    reason: CreateConstError::NeedsAnonConst
                }]
            ),
            "only `W<S>` is an error of lowering: {:?}",
            lowered.diagnostics(),
        );
        assert_eq!(
            lowered.defined_anon_consts().len(),
            2,
            "`W<true>` and `[u8; S]` are anon consts"
        );
    });
}
