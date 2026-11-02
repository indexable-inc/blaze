//! [`BodySyntax::tree`] against the anchor it is of, and the thread's tree cache.

use syntax::view;
use hir_expand::fixture;

use crate::{
    AssocItemId, DefWithBodyId, ModuleDefId,
    expr_store::body::body_syntax,
    nameres::{assoc::ImplItems, crate_def_map},
    test_db::TestDB,
};

const CODE: &str = r#"
struct S;

impl S {
    fn a(&self) -> u8 { 1 }
    fn b(x: u8) -> u8 { x + 2 }
}

fn free() {
    fn nested() {}
    let _ = 3;
}

const K: u8 = { 4 };
static V: u8 = K + 1;

mod m {
    pub fn deep() -> u8 { 6 }
}
"#;

fn bodies(db: &TestDB) -> Vec<DefWithBodyId> {
    let mut bodies = Vec::new();
    let krate = *base_db::all_crates(db).last().expect("a crate");
    for (_, module) in crate_def_map(db, krate).modules(db) {
        let scope = module.scope(db);
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(it) => bodies.push(it.into()),
                ModuleDefId::ConstId(it) => bodies.push(it.into()),
                ModuleDefId::StaticId(it) => bodies.push(it.into()),
                _ => {}
            }
        }
        for imp in scope.impls() {
            for (_, item) in &ImplItems::of(db, imp).0.items {
                if let AssocItemId::FunctionId(it) = *item {
                    bodies.push(it.into());
                }
            }
        }
    }
    bodies
}

/// The tree is the anchor's: a root of the anchor's kind with its text from offset 0, in which
/// the item's pointer finds a node of the item's kind and range.
#[test]
fn tree_is_the_anchor() {
    let db = fixture::db(CODE).into_db();
    let bodies = bodies(&db);
    assert!(bodies.len() >= 6, "{bodies:?}");
    for def in bodies {
        let syntax = body_syntax(&db, def).as_ref().expect("a body written in a file");
        let tree = syntax.tree(&db);
        let root = view::Node::root(&tree);
        assert_eq!(root.kind(), syntax.kind, "{def:?}");
        assert_eq!(root.text(), syntax.text(), "{def:?}");
        assert_eq!(root.text_range().start(), syntax::TextSize::new(0), "{def:?}");
        let item = syntax::view::to_node(syntax.item, root);
        assert_eq!((item.kind(), item.text_range()), (syntax.item.kind(), syntax.item.text_range()));
    }
}

/// `RA_CHECK_RELATIVE_BODIES` parses every tree again, so there is nothing kept to compare.
#[test]
fn tree_is_kept_on_its_thread() {
    if super::check_relative() {
        return;
    }
    let db = fixture::db(CODE).into_db();
    for def in bodies(&db) {
        let syntax = body_syntax(&db, def).as_ref().expect("a body written in a file");
        let (first, second) = (syntax.tree(&db), syntax.tree(&db));
        assert!(std::sync::Arc::ptr_eq(&first, &second), "{def:?}");
    }
}
