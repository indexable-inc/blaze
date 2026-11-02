//! The imports of traits a crate's inference picked through, rustc's `used_trait_imports`.

use base_db::{Crate, Db, all_crates};
use hir_def::{
    Lookup as _,
    item_scope::{GlobId, ImportId, ImportOrExternCrate},
    src::use_tree_to_view,
};
use hir_expand::fixture;

use super::used_imports_with_traits;

const FIXTURE: &str = r#"
//- /lib.rs crate:main
mod m {
    pub trait Method { fn method(&self) {} }
    impl Method for u8 {}
    pub trait Assoc { fn assoc() {} }
    impl Assoc for u8 {}
    pub trait Pattern { const C: u8; }
    impl Pattern for u8 { const C: u8 = 0; }
    pub trait Unused { fn other(&self) {} }
    impl Unused for u8 {}
}
use m::Method;
use m::Assoc;
use m::Unused;
fn by_method() {
    0u8.method();
}
fn by_path() {
    use m::Assoc as _;
    u8::assoc();
}
fn by_pattern(x: u8) {
    use m::Pattern;
    match x {
        u8::C => {}
        _ => {}
    }
}
"#;

fn main_crate(db: &Db) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == "main"))
        .unwrap()
}

#[test]
fn the_innermost_import_of_each_picked_trait_is_used() {
    let db = fixture::db(FIXTURE).into_db();
    let used = used_imports_with_traits(&db, main_crate(&db));
    let mut texts: Vec<String> = used
        .imports()
        .filter_map(|(import, _)| match import {
            ImportOrExternCrate::Import(ImportId { use_, idx })
            | ImportOrExternCrate::Glob(GlobId { use_, idx }) => {
                Some(use_tree_to_view(&db, use_.lookup(&db).id, idx).to_string())
            }
            ImportOrExternCrate::ExternCrate(_) => None,
        })
        .collect();
    texts.sort();
    // The block's `use m::Assoc as _` is the nearer import of `Assoc`, so the module's is unused;
    // `Unused` is implemented for `u8` but never picked.
    assert_eq!(texts, ["m::Assoc as _", "m::Method", "m::Pattern"]);
}
