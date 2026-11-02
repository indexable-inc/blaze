//! `utils::pat_has_ref_mut_binding` (rustc's), which inference and MIR lowering's deref
//! patterns both ask, gives what each of their copies gave: a pattern binds by `ref mut` when
//! it says so or match ergonomics makes it.

use base_db::{Db, all_crates};
use hir_def::{
    DefWithBodyId, FunctionId, ModuleDefId,
    expr_store::{Body, ExpressionStore},
    hir::{Pat, PatId},
    nameres::crate_def_map,
    signatures::FunctionSignature,
};
use rustc_ast_ir::Mutability;
use hir_expand::fixture;

use crate::{BindingMode, ByRef, InferBodyId, InferenceResult, utils::pat_has_ref_mut_binding};

const FIXTURE: &str = r#"
//- minicore: option
fn f(mut o: Option<u8>, r: &mut (u8, u8), s: &(u8, u8)) {
    if let Some(ref mut x) = o {
        *x = 1;
    }
    let (a, b) = r;
    let (c, d) = s;
    let (e, ref g) = (1u8, 2u8);
    let _ = (a, b, c, d, e, g);
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

/// The copies inference and MIR lowering had, one body over either's binding modes.
fn reference(store: &ExpressionStore, pat: PatId, mode: impl Fn(PatId) -> Option<BindingMode>) -> bool {
    let mut found = false;
    store.walk_pats(pat, &mut |pat| {
        if let Some(BindingMode(ByRef::Yes(Mutability::Mut), _)) = mode(pat) {
            found = true;
        }
    });
    found
}

#[test]
fn a_pattern_binds_by_ref_mut_as_both_old_copies_said() {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let def = DefWithBodyId::FunctionId(function(&db, "f"));
        let body = Body::of(&db, def);
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        let mode = |pat| infer.binding_mode(pat);
        let mut named = Vec::new();
        for (pat, it) in body.pats() {
            let has = pat_has_ref_mut_binding(&body, pat, mode);
            assert_eq!(has, reference(&body, pat, mode), "{it:?}");
            if let Pat::Bind { id, .. } = it {
                named.push((body[id].name.as_str().to_owned(), has));
            }
        }
        named.sort();
        let by_ref_mut = ["a", "b", "x"];
        let expected: Vec<(String, bool)> = ["a", "b", "c", "d", "e", "g", "o", "r", "s", "x"]
            .iter()
            .map(|&name| (name.to_owned(), by_ref_mut.contains(&name)))
            .collect();
        assert_eq!(named, expected);
    });
}
