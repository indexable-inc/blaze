//! `&'static str` as a const generic parameter (`#![feature(adt_const_params,
//! unsized_const_params)]`, the pinned nightly's features; row Q5 of
//! docs/v2/plan/design/sql.md): the parameter's type is refused without both features and
//! when it names another parameter; an argument is a value tree of its bytes, so two arguments
//! with equal bytes are one type in every crate and two with different bytes two; a mismatch
//! shows both strings, a long one cut; an argument left out is inferred. Arguments that are
//! evaluated (a `const`, `concat!`, a const fn's result) are tested in hir-ty-mir, which
//! builds the MIR their evaluation needs (`tests::const_str` there).

use base_db::{Crate, Db, all_crates};
use hir_def::{
    AdtId, DefWithBodyId, GenericDefId, ModuleDefId, VariantId,
    expr_store::Body,
    hir::{Pat, generics::GenericParams},
    nameres::crate_def_map,
    signatures::{FunctionSignature, StructSignature},
};
use hir_expand::fixture;

use crate::{
    InferBodyId, InferenceDiagnostic, InferenceResult,
    const_param_ty::{ConstParamTyReason, const_param_ty_errors},
    const_valtree::STR_SHOWN,
    db::HirDatabase,
    display::{DisplayTarget, HirDisplay},
    next_solver::Ty,
};

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap_or_else(|| panic!("no crate `{name}` in the fixture"))
}

fn struct_in(db: &Db, krate: Crate, name: &str) -> hir_def::StructId {
    crate_def_map(db, krate)
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .find_map(|def| match def {
            ModuleDefId::AdtId(AdtId::StructId(it))
                if StructSignature::of(db, it).name.as_str() == name =>
            {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no struct `{name}` in the crate"))
}

fn function_in(db: &Db, krate: Crate, name: &str) -> hir_def::FunctionId {
    crate_def_map(db, krate)
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .find_map(|def| match def {
            ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => {
                Some(it)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no function `{name}` in the crate"))
}

/// The field types of `krate`'s struct `V`, in order.
fn field_tys<'db>(db: &'db Db, krate: Crate) -> Vec<Ty<'db>> {
    let v = struct_in(db, krate, "V");
    db.field_types_with_diagnostics(VariantId::StructId(v))
        .value
        .as_slice()
        .iter()
        .map(|it| it.ty().instantiate_identity().skip_norm_wip())
        .collect()
}

/// Each refused const parameter of the struct `name` of the test crate, by name.
fn refused(db: &Db, name: &str) -> Vec<(String, ConstParamTyReason)> {
    let def = GenericDefId::AdtId(struct_in(db, fixture::test_crate(db), name).into());
    let params = GenericParams::of(db, def);
    const_param_ty_errors(db, def)
        .iter()
        .map(|error| {
            let name = params[error.param.local_id()].name().expect("a const parameter is named");
            (name.as_str().to_owned(), error.reason)
        })
        .collect()
}

const PARAMS: &str = r#"
struct Str<const S: &'static str>;
struct Bytes<const B: &'static [u8]>;
struct Pair<const P: (&'static str, u8)>;
struct Mut<const M: &'static mut str>;
struct NotConst;
struct Owned<const O: NotConst>;
struct Lifetime<'a, const L: &'a str>;
"#;

fn params_db(features: &str) -> Db {
    let attr = match features {
        "" => String::new(),
        features => format!("#![feature({features})]\n"),
    };
    fixture::db(&format!("//- minicore: sized, const_param_ty\n{attr}{PARAMS}")).into_db()
}

#[test]
fn a_str_parameter_needs_adt_const_params() {
    let db = params_db("");
    blaze_query::db::attach(&db, || {
        assert_eq!(refused(&db, "Str"), [("S".to_owned(), ConstParamTyReason::NotSupported)]);
        assert_eq!(
            refused(&db, "Lifetime"),
            [("L".to_owned(), ConstParamTyReason::DependsOnGenerics)],
            "E0770 whatever the features"
        );
    });
}

#[test]
fn a_str_parameter_needs_unsized_const_params_too() {
    let db = params_db("adt_const_params");
    blaze_query::db::attach(&db, || {
        for (name, param) in [("Str", "S"), ("Bytes", "B"), ("Pair", "P"), ("Mut", "M")] {
            assert_eq!(
                refused(&db, name),
                [(param.to_owned(), ConstParamTyReason::UnsizedConstParams)],
                "`{name}`: core's `ConstParamTy_` impls of `str`, `[T]` and `&T` are bounded \
                 on `unsized_const_params`"
            );
        }
        assert_eq!(refused(&db, "Owned"), [("O".to_owned(), ConstParamTyReason::NotConstParamTy)]);
    });
}

#[test]
fn with_both_features_str_and_byte_slices_are_allowed() {
    let db = params_db("adt_const_params, unsized_const_params");
    blaze_query::db::attach(&db, || {
        assert_eq!(refused(&db, "Str"), []);
        assert_eq!(refused(&db, "Bytes"), [], "`&'static [u8]` takes the same path as `&str`");
        assert_eq!(refused(&db, "Pair"), []);
        assert_eq!(
            refused(&db, "Mut"),
            [("M".to_owned(), ConstParamTyReason::NotConstParamTy)],
            "core has no `ConstParamTy_` for `&mut T`"
        );
        assert_eq!(refused(&db, "Owned"), [("O".to_owned(), ConstParamTyReason::NotConstParamTy)]);
        assert_eq!(
            refused(&db, "Lifetime"),
            [("L".to_owned(), ConstParamTyReason::DependsOnGenerics)]
        );
    });
}

const TYPES: &str = r#"
//- minicore: sized, const_param_ty
//- /lib.rs crate:lib
#![feature(adt_const_params, unsized_const_params)]
pub struct S<const X: &'static str>;
//- /a.rs crate:a deps:lib
#![feature(adt_const_params, unsized_const_params)]
use lib::S;
struct V {
    abc: S<"abc">,
    braced: S<{ "abc" }>,
    abd: S<"abd">,
    empty: S<"">,
    escaped: S<"a\x62c">,
    raw: S<r"abc">,
}
//- /b.rs crate:b deps:lib
#![feature(adt_const_params, unsized_const_params)]
struct V {
    abc: lib::S<"abc">,
}
"#;

#[test]
fn two_arguments_with_equal_bytes_are_one_type() {
    let db = fixture::db(TYPES).into_db();
    blaze_query::db::attach(&db, || {
        let a = field_tys(&db, crate_named(&db, "a"));
        let [abc, braced, abd, empty, escaped, raw] = a[..] else { panic!("{a:?}") };
        assert_eq!(abc, braced, "`{{ \"abc\" }}` is the literal");
        assert_eq!(abc, escaped, "by the bytes, not the spelling");
        assert_eq!(abc, raw);
        assert_ne!(abc, abd, "other bytes are another type");
        assert_ne!(abc, empty);
        let b = field_tys(&db, crate_named(&db, "b"));
        assert_eq!(b[0], abc, "two crates naming `S<\"abc\">` name one type");
    });
}

const BODIES: &str = r#"
//- minicore: sized, const_param_ty
#![feature(adt_const_params, unsized_const_params)]
pub struct S<const X: &'static str>;
pub fn take<const X: &'static str>(s: S<X>) -> S<X> { s }
pub fn mismatch() {
    let _: S<"abc"> = S::<"abd">;
}
pub fn long() {
    let _: S<"abc"> = S::<"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx">;
}
pub fn inferred() {
    let r = take(S::<"abc">);
    let s: S<_> = S::<"def">;
}
"#;

/// The type mismatches of `name`'s body, each as `(expected, found)` displayed as a
/// diagnostic shows them.
fn mismatches(db: &Db, name: &str) -> Vec<(String, String)> {
    let krate = fixture::test_crate(db);
    let target = DisplayTarget::from_crate(db, krate);
    let def = DefWithBodyId::FunctionId(function_in(db, krate, name));
    InferenceResult::of(db, InferBodyId::DefWithBodyId(def))
        .diagnostics()
        .iter()
        .filter_map(|it| match it {
            InferenceDiagnostic::TypeMismatch { expected, found, .. } => Some((
                expected.as_ref().display(db, target).to_string(),
                found.as_ref().display(db, target).to_string(),
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn a_mismatch_shows_both_strings() {
    let db = fixture::db(BODIES).into_db();
    blaze_query::db::attach(&db, || {
        assert_eq!(mismatches(&db, "mismatch"), [(r#"S<"abc">"#.to_owned(), r#"S<"abd">"#.to_owned())]);
    });
}

#[test]
fn a_long_string_is_shown_cut_with_its_length() {
    let db = fixture::db(BODIES).into_db();
    blaze_query::db::attach(&db, || {
        let shown = format!("S<\"{}…\" (100 bytes)>", "x".repeat(STR_SHOWN));
        assert_eq!(mismatches(&db, "long"), [(r#"S<"abc">"#.to_owned(), shown)]);
    });
}

#[test]
fn an_argument_left_out_is_inferred() {
    let db = fixture::db(BODIES).into_db();
    blaze_query::db::attach(&db, || {
        let krate = fixture::test_crate(&db);
        let target = DisplayTarget::from_crate(&db, krate);
        let def = DefWithBodyId::FunctionId(function_in(&db, krate, "inferred"));
        let body = Body::of(&db, def);
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        assert!(infer.diagnostics().is_empty(), "{:?}", infer.diagnostics());
        let shown = |binding: &str| {
            let pat = body
                .pats()
                .find_map(|(pat, it)| match it {
                    Pat::Bind { id, .. } if body[id].name.as_str() == binding => Some(pat),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("the binding `{binding}`"));
            infer.pat_ty(pat).display(&db, target).to_string()
        };
        assert_eq!(shown("r"), r#"S<"abc">"#, "`take`'s `X` from its argument");
        assert_eq!(shown("s"), r#"S<"def">"#, "`_` from the value");
    });
}
