//! A coercion LUB over arms of a projection type, in a generic body: the lattice relation
//! normalizes an alias it meets unnormalized instead of panicking.

use base_db::{Db, all_crates};
use hir_def::{
    DefWithBodyId, FunctionId, ModuleDefId, nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::{InferBodyId, InferenceResult};

const FIXTURE: &str = r#"
//- minicore: sized
trait Tr {
    type Item;
    fn make(&self) -> Self::Item;
}
fn f<T: Tr>(t: &T, x: T::Item, b: bool) -> T::Item {
    let y = if b { x } else { t.make() };
    match b {
        true => y,
        false => <T as Tr>::make(t),
    }
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
fn arms_of_a_projection_type_find_their_lub() {
    let db = fixture::db(FIXTURE).into_db();
    crate::next_solver::attach_db(&db, || {
        let def = DefWithBodyId::FunctionId(function(&db, "f"));
        let infer = InferenceResult::of(&db, InferBodyId::DefWithBodyId(def));
        assert!(infer.diagnostics().is_empty(), "{:?}", infer.diagnostics());
    });
}
