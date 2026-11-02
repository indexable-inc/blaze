//! Allocation budgets (`blaze-alloc-count`) of reads of what the maintainer published (design
//! §12, "no allocation on a read"): once warm, a name looked up in a published module
//! (`ScopeSlot::Input`, the tracked per-name memo of its `ScopeInput`), its scope, children,
//! textual macros and traits, and a name read through the crate's published `DefMap`, each
//! allocate nothing; also after an edit of another module, once warm again.

use std::hint::black_box;

use base_db::{Crate, Db};
use blaze_alloc_count::assert_at_most;
use hir_expand::name::Name;

use super::{Loaded, module_at, name};
use crate::{
    ModuleId,
    nameres::{ScopeSlot, crate_def_map, crate_local_def_map},
};

const CODE: &str = r#"
//- /lib.rs
pub mod a {
    pub struct C;
    pub fn g() {}
    pub use self::C as Renamed;
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
    pub struct W10;
    pub struct W11;
}
pub mod other;
pub use a::*;
macro_rules! local { () => {} }
pub trait Tr {}
//- /other.rs
pub struct O;
"#;

fn names(texts: &[&str]) -> Vec<Name> {
    texts.iter().map(|it| name(it)).collect()
}

/// Every read the budgets measure, of the root and `wide` of the fixture.
fn reads(db: &Db, krate: Crate, wide_id: ModuleId, at_root: &[Name], in_wide: &[Name]) {
    let map = crate_def_map(db, krate);
    let root = map.module(db, map.root);
    let wide = map.module(db, wide_id);
    for it in at_root {
        black_box(root.name_in_scope(db, it));
        black_box(map.name_in_module(db, map.root, it));
    }
    for it in in_wide {
        black_box(wide.name_in_scope(db, it));
        black_box(map.name_in_module(db, wide_id, it));
    }
    black_box(root.scope(db));
    black_box(root.children(db).len());
    black_box(root.legacy_macro_in_scope(db, &at_root[0]));
    black_box(root.traits_in_scope(db).count());
}

#[test]
fn warm_reads_of_published_scopes_allocate_nothing() {
    let mut loaded = Loaded::new(CODE);
    let krate = loaded.krate();
    {
        let db = &loaded.db;
        let map = crate_local_def_map(db, krate).def_map(db);
        super::assert_published(db, map, "the fixture");
        for (id, data) in map.modules(db) {
            assert!(matches!(data.scope, ScopeSlot::Input(_)), "{id:?} is not read from its input");
        }
    }
    let at_root = names(&["local", "a", "wide", "other", "C", "Renamed", "g", "Tr", "missing"]);
    // alloc: test setup, outside the measure
    let in_wide: Vec<Name> = (0..12).map(|i| name(&format!("W{i}"))).chain([name("missing")]).collect();
    assert!(
        crate_def_map(&loaded.db, krate).module(&loaded.db, module_at(&loaded.db, krate, &["wide"])).scope(&loaded.db).entries().count() > 8,
        "`wide` does not reach the scope's index"
    );

    let wide = module_at(&loaded.db, krate, &["wide"]);
    reads(&loaded.db, krate, wide, &at_root, &in_wide);
    assert_at_most(0, "warm reads of published scopes", || reads(&loaded.db, krate, wide, &at_root, &in_wide));

    // Another module edited: the root's and `wide`'s inputs are as they were.
    let other = loaded.file("/other.rs");
    loaded.db.set_file_text(other, "pub struct O;\npub struct P;\n");
    let wide = module_at(&loaded.db, krate, &["wide"]);
    reads(&loaded.db, krate, wide, &at_root, &in_wide);
    assert_at_most(0, "warm reads after an edit of another module", || {
        reads(&loaded.db, krate, wide, &at_root, &in_wide)
    });
}
