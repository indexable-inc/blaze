//! Allocation budgets of reading a collected scope: a settled scope has its tables, so every read
//! (names in order, resolutions, traits, the name of a def) allocates nothing, from the first
//! read on.

use std::hint::black_box;

use base_db::relevant_crates;
use blaze_alloc_count::assert_at_most;
use hir_expand::name::Name;
use intern::Symbol;
use hir_expand::fixture;

use crate::{
    ModuleDefId,
    item_scope::{ItemInNs, LINEAR},
    nameres::crate_def_map,
};

const CODE: &str = r#"
pub mod small {
    pub struct A;
    pub fn b() {}
    pub trait T {}
}
pub mod wide {
    pub struct W0;
    pub struct W1;
    pub struct W2;
    pub struct W3;
    pub struct W4;
    pub struct W5;
    pub struct W6;
    pub struct W7;
    pub struct W8;
    pub struct W9;
    pub trait Tr {}
    pub use super::small::A as Again;
    pub use super::small::T as _;
}
"#;

fn name(text: &str) -> Name {
    Name::new_symbol_root(Symbol::intern(text))
}

/// Budget: `entries`, `resolutions`, `traits` and `name_of` of every type in a scope of at most
/// [`LINEAR`] names and one of more, with no read before the measure: 0 allocations.
#[test]
fn reading_a_settled_scope_allocates_nothing() {
    let fx = fixture::db(CODE);
    let file = fx.only_file();
    let db = fx.into_db();
    let krate = relevant_crates(&db, file.file_id(&db))[0];
    let map = crate_def_map(&db, krate);
    let root = map.module(&db, map.root).scope(&db);
    let module = |text: &str| match root.get(&name(text)).take_types() {
        Some(ModuleDefId::ModuleId(it)) => map.module(&db, it).scope(&db),
        other => panic!("`{text}` is not a module of the root: {other:?}"),
    };
    let scopes = [module("small"), module("wide")];
    let items: Vec<ItemInNs> = scopes
        .iter()
        .flat_map(|scope| scope.types().map(|(_, it)| ItemInNs::Types(it.def)))
        .collect();

    let counts = assert_at_most(0, "reading a settled scope", || {
        scopes.map(|scope| {
            for &item in &items {
                black_box(scope.name_of(item));
            }
            (scope.entries().count(), scope.resolutions().count(), scope.traits().count())
        })
    });
    assert_eq!(counts[0], (3, 3, 1), "`small`: three names, one trait");
    assert!(counts[1].0 > LINEAR, "`wide` does not reach the index: {:?}", counts[1]);
    assert_eq!(counts[1].1, counts[1].0 + 1, "`wide`'s resolutions: its names and `T as _`");
    assert_eq!(counts[1].2, 2, "`wide`'s traits: `Tr` and `T as _`");
}
