//! A trait's function reached through an imported name (`use Trait::f;`,
//! `import_trait_associated_functions`) is rustc's `<_ as Trait>::f`: its `Self` is inferred
//! from the call's use, for a trait of another crate and of the crate itself.

use base_db::{Db, all_crates};
use hir_def::{
    DefWithBodyId, FunctionId, ModuleDefId, expr_store::Body, nameres::crate_def_map,
    signatures::FunctionSignature,
};
use rustc_type_ir::{UintTy, inherent::IntoKind as _};
use hir_expand::fixture;

use crate::{InferBodyId, InferenceResult, next_solver::TyKind};

const FIXTURE: &str = r#"
//- minicore: default, builtin_impls
#![feature(import_trait_associated_functions)]
use core::default::Default::default;
use crate::Make::make;

trait Make {
    fn make() -> Self;
}
impl Make for u16 {
    fn make() -> Self {
        0
    }
}

fn foreign() {
    let x = default();
    let _y: u8 = x;
}
fn local() {
    let x = make();
    let _y: u16 = x;
}
"#;

fn function(db: &Db, name: &str) -> FunctionId {
    for &krate in all_crates(db).iter() {
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            for def in module.scope(db).declarations() {
                if let ModuleDefId::FunctionId(it) = def
                    && FunctionSignature::of(db, it).name.as_str() == name
                {
                    return it;
                }
            }
        }
    }
    panic!("no function `{name}` in the fixture")
}

#[test]
fn an_imported_trait_function_infers_its_self_from_the_call() {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        for (name, expected) in [("foreign", UintTy::U8), ("local", UintTy::U16)] {
            let def = DefWithBodyId::FunctionId(function(&db, name));
            let body = Body::of(&db, def);
            let (x, _) = body
                .bindings()
                .find(|(_, it)| it.name.as_str() == "x")
                .unwrap_or_else(|| panic!("no binding `x` in `{name}`"));
            let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
            let ty = infer.binding_ty(x);
            assert!(
                matches!(ty.kind(), TyKind::Uint(it) if it == expected),
                "{name}: `x` is {ty:?}, not {expected:?}"
            );
            assert!(infer.diagnostics().is_empty(), "{name}: {:?}", infer.diagnostics());
        }
    });
}
