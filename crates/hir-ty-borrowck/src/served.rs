//! Bodies of a dependency served from its part: a served body's MIR is the MIR of its own
//! inference, and a served verdict is the gate's answer without a borrow check.

use std::sync::Arc;

use base_db::{Crate, Db, all_crates};
use blaze_format::Aligned;
use blaze_hash::HashMap;
use hir_def::{
    DefWithBodyId, LoweringMode, expr_store::Body, nameres::demand::crate_body_owners,
    signatures::FunctionSignature,
};
use hir_expand::fixture;
use hir_expand::name::Name;
use hir_ty::{InferBodyId, check_cache, db::HirDatabase, infer_query_with_inspect};
use hir_ty_mir::mir::lower_body_to_mir;

use crate::checked;

const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, add, fn, option, deref, derive, eq
//- /dep.rs crate:dep deps:core new_source_root:library
#[derive(Clone, Copy, PartialEq)]
pub struct Pair { pub a: u32, pub b: Option<u32> }
pub fn sum(pair: &Pair) -> u32 { match pair.b { Some(b) => pair.a + b, None => pair.a } }
pub fn grow(pair: &mut Pair, by: u32) { pair.a += by; if let Some(b) = &mut pair.b { *b += by; } }
pub fn closures(mut pair: Pair) -> u32 {
    let mut bump = |by: u32| pair.a += by;
    bump(1);
    let read = move || pair.a;
    read()
}
pub fn same(pair: Pair) -> bool { pair == pair.clone() } // clone: the fixture's code
pub fn two_mut() { let mut n = 0; let a = &mut n; let b = &mut n; *a += 1; *b += 1; }
pub const C: u32 = 3 + 4;
//- /main.rs crate:main deps:core,dep new_source_root:local
fn main() { let _ = dep::sum(&dep::Pair { a: 1, b: None }); }
"#;

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        // alloc: test setup
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap()
}

/// A database whose `dep` is served from a part written in another one, every body marked by
/// `passed`.
fn served(passed: fn(DefWithBodyId) -> bool) -> (Db, Crate) {
    let first = fixture::db(FIXTURE).into_db();
    let bytes = check_cache::encode(&first, crate_named(&first, "dep"), &passed).unwrap();
    let mut db = fixture::db(FIXTURE).into_db();
    let dep = crate_named(&db, "dep");
    // alloc: test setup
    let mut parts = HashMap::default();
    // alloc: test setup
    parts.insert(dep, Arc::new(Aligned::copy_from(&bytes)) as hir_def::deps::ImageBuf);
    check_cache::set_served(&mut db, parts);
    (db, dep)
}

#[test]
fn a_served_body_lowers_to_the_mir_of_its_own_inference() {
    let (db, dep) = served(|_| true);
    let owners = crate_body_owners(&db, dep);
    assert!(owners.len() >= 6, "the fixture's bodies: {owners:?}");
    for def in owners {
        let owner = InferBodyId::DefWithBodyId(def);
        let served = db.mir_body(owner);
        assert!(served.is_ok(), "{def:?} lowers");
        let own = infer_query_with_inspect(&db, def, None, LoweringMode::Analysis);
        let body = Body::of(&db, def);
        let lowered = lower_body_to_mir(
            &db,
            owner,
            body,
            &own,
            body.root_expr(),
            body.self_param.map(|param| param.formal),
            &body.params,
        );
        assert!(served.ok() == lowered.as_ref().ok(), "{def:?}'s MIR");
    }
}

#[test]
fn a_served_verdict_skips_the_borrow_check() {
    let two_mut = |db: &Db, dep| {
        crate_body_owners(db, dep)
            .into_iter()
            .find(|&def| match def {
                DefWithBodyId::FunctionId(it) => {
                    FunctionSignature::of(db, it).name == Name::new_root("two_mut")
                }
                _ => false,
            })
            .unwrap()
    };
    let own = fixture::db(FIXTURE).into_db();
    let own_dep = crate_named(&own, "dep");
    assert!(checked(&own, two_mut(&own, own_dep)).is_err(), "the body has a borrow error");

    let (db, dep) = served(|_| true);
    assert!(check_cache::served_passed(&db, two_mut(&db, dep)));
    assert!(checked(&db, two_mut(&db, dep)).is_ok(), "the served verdict is the gate's answer");

    let (db, dep) = served(|_| false);
    assert!(!check_cache::served_passed(&db, two_mut(&db, dep)));
    assert!(checked(&db, two_mut(&db, dep)).is_err(), "a body that did not pass is checked");
}
