//! rustc's `check_param_wf` of a const parameter's type: an integer, `bool` or `char` only,
//! unless `adt_const_params` asks for any type implementing `ConstParamTy`.

use base_db::Db;
use hir_def::{
    AdtId, GenericDefId, ModuleDefId, hir::generics::GenericParams, nameres::crate_def_map,
    signatures::StructSignature,
};
use hir_expand::fixture;

use crate::const_param_ty::{ConstParamTyReason, const_param_ty_errors};

fn struct_named(db: &Db, name: &str) -> GenericDefId {
    crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .find_map(|def| match def {
            ModuleDefId::AdtId(AdtId::StructId(it))
                if StructSignature::of(db, it).name.as_str() == name =>
            {
                Some(GenericDefId::AdtId(it.into()))
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no struct `{name}` in the fixture"))
}

/// Each refused parameter of `def`, by name.
fn refused(db: &Db, def: GenericDefId) -> Vec<(String, ConstParamTyReason)> {
    let params = GenericParams::of(db, def);
    const_param_ty_errors(db, def)
        .iter()
        .map(|error| {
            let name = params[error.param.local_id()].name().expect("a const parameter is named");
            (name.as_str().to_owned(), error.reason)
        })
        .collect()
}

#[test]
fn without_the_feature_only_integers_bool_and_char_are_allowed() {
    let db = fixture::db(
        r#"
//- minicore: sized
struct S<
    const A: f32,
    const B: fn(),
    const C: *const u8,
    const D: usize,
    const E: bool,
    const F: char,
    const G: i8,
    const H: &'static str,
>;
struct Unknown<const X: Missing>;
"#,
    )
    .into_db();
    crate::next_solver::attach_db(&db, || {
        assert_eq!(
            refused(&db, struct_named(&db, "S")),
            [
                ("A".to_owned(), ConstParamTyReason::NotSupported),
                ("B".to_owned(), ConstParamTyReason::FnPtr),
                ("C".to_owned(), ConstParamTyReason::RawPtr),
                ("H".to_owned(), ConstParamTyReason::NotSupported),
            ]
        );
        assert_eq!(
            refused(&db, struct_named(&db, "Unknown")),
            [],
            "a type that does not resolve is reported where it is written, not again here"
        );
    });
}

#[test]
fn with_adt_const_params_the_type_must_implement_const_param_ty() {
    let db = fixture::db(
        r#"
//- minicore: sized, const_param_ty
#![feature(adt_const_params)]
#[derive(PartialEq, Eq)]
struct Good;
impl core::marker::ConstParamTy_ for Good {}
struct Bad;
struct S<const A: Good, const B: Bad>;
"#,
    )
    .into_db();
    crate::next_solver::attach_db(&db, || {
        assert_eq!(
            refused(&db, struct_named(&db, "S")),
            [("B".to_owned(), ConstParamTyReason::NotConstParamTy)]
        );
    });
}

#[test]
fn with_adt_const_params_and_no_lang_item_the_lang_item_is_missing() {
    let db = fixture::db(
        r#"
//- minicore: sized
#![feature(adt_const_params)]
struct S<const A: u8>;
"#,
    )
    .into_db();
    crate::next_solver::attach_db(&db, || {
        assert_eq!(
            refused(&db, struct_named(&db, "S")),
            [("A".to_owned(), ConstParamTyReason::MissingLangItem)]
        );
    });
}
