//! Bodies of a file parsed as an outline (#1294): a dependency crate's file, whose `fn` bodies
//! are parsed only when a body is lowered.

use base_db::{Crate, all_crates};
use hir_expand::name::Name;
use span::Edition;
use syntax::FlatTree;
use hir_expand::fixture;

use crate::{
    AssocItemId, DefWithBodyId, ModuleDefId,
    expr_store::body::{Body, body_syntax},
    nameres::{
        assoc::{ImplItems, TraitItems},
        crate_def_map,
    },
    test_db::TestDB,
};

const CODE: &str = r#"
pub struct S { x: u32 }

// With an attribute, the `impl` is the anchor of its `fn`s' bodies, not each `fn` its own.
#[allow(dead_code)]
impl S {
    pub fn get(&self) -> u32 { self.x }
    pub fn with(&self, y: u32) -> u32 {
        let z = self.get() + y;
        z * 2
    }
}

pub trait T {
    fn d(&self) -> u32 { 1 }
    fn no_body(&self);
}

pub fn free(a: u32) -> u32 {
    fn inner(b: u32) -> u32 { b + 1 }
    match a {
        0 => inner(a),
        n => {
            let c = || n;
            c()
        }
    }
}

pub fn attributed() {
    #![allow(unused)]
    let _ = 5;
}

pub const C: u32 = { 3 };
pub static ST: u32 = C + 1;

pub mod m {
    pub fn deep() -> &'static str {
        // a comment first
        "deep"
    }
}
"#;

/// The same code as a library crate, whose file is an outline, and as the crate that uses it,
/// whose file is parsed whole.
fn db() -> TestDB {
    fixture::db(&format!(
        "//- /dep.rs crate:dep new_source_root:library\n{CODE}\n\
         //- /main.rs crate:main deps:dep new_source_root:local\n{CODE}\n"
    ))
    .into_db()
}

fn krate(db: &TestDB, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|krate| {
            krate.extra_data(db).display_name.as_ref().map(|it| it.canonical_name().as_str())
                == Some(name)
        })
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

/// Every body of `krate` with what it is named by, in the order the crate declares them.
fn bodies(db: &TestDB, krate: Crate) -> Vec<(String, DefWithBodyId)> {
    fn assoc(bodies: &mut Vec<(String, DefWithBodyId)>, items: &[(Name, AssocItemId)]) {
        for (name, item) in items {
            let def: DefWithBodyId = match *item {
                AssocItemId::FunctionId(it) => it.into(),
                AssocItemId::ConstId(it) => it.into(),
                AssocItemId::TypeAliasId(_) => continue,
            };
            bodies.push((name.as_str().to_owned(), def));
        }
    }
    let mut bodies = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        let scope = module.scope(db);
        for decl in scope.declarations() {
            let (name, def): (String, DefWithBodyId) = match decl {
                ModuleDefId::FunctionId(it) => (format!("{it:?}"), it.into()),
                ModuleDefId::ConstId(it) => (format!("{it:?}"), it.into()),
                ModuleDefId::StaticId(it) => (format!("{it:?}"), it.into()),
                ModuleDefId::TraitId(it) => {
                    assoc(&mut bodies, &TraitItems::query(db, it).items);
                    continue;
                }
                _ => continue,
            };
            bodies.push((name, def));
        }
        for imp in scope.impls() {
            assoc(&mut bodies, &ImplItems::of(db, imp).0.items);
        }
    }
    bodies
}

/// The tree of the file `def` is written in.
fn file_tree(db: &TestDB, def: DefWithBodyId) -> &FlatTree {
    let syntax = body_syntax(db, def).as_ref().expect("a body written in a file");
    let file = syntax.file_id.file_id().expect("a body written in a file");
    file.parse(db).root().shared_file()
}

#[test]
fn an_outline_lowers_every_body_as_a_full_parse() {
    let db = db();
    let (dep, main) = (bodies(&db, krate(&db, "dep")), bodies(&db, krate(&db, "main")));
    assert_eq!(dep.len(), main.len(), "the two crates have the same bodies");
    assert!(dep.len() >= 9, "{dep:?}");
    let (&(_, first_dep), &(_, first_main)) = (&dep[0], &main[0]);
    assert!(
        file_tree(&db, first_dep).lazies().next().is_some(),
        "the library crate's file is not parsed as an outline (base-db's parse query)"
    );
    assert!(file_tree(&db, first_main).lazies().next().is_none(), "the local crate's file is");

    let item_tree = |def, krate| {
        let syntax = body_syntax(&db, def).as_ref().expect("a body written in a file");
        crate::file_item_tree(&db, syntax.file_id, krate).pretty_print(&db, Edition::CURRENT)
    };
    assert_eq!(
        item_tree(first_dep, krate(&db, "dep")),
        item_tree(first_main, krate(&db, "main")),
        "the item tree of an outline, inner attributes of bodies included"
    );

    for ((_, in_dep), (name, in_main)) in dep.iter().zip(&main) {
        let syntax = body_syntax(&db, *in_dep).as_ref().expect("a body written in a file");
        assert!(syntax.reparses_alone(), "{name}: the outline's anchor parses differently alone");
        let printed = |def| Body::of(&db, def).pretty_print(&db, def, Edition::CURRENT);
        assert_eq!(printed(*in_dep), printed(*in_main), "{name}");
    }
}

#[test]
fn lowering_one_fn_parses_only_its_own_body() {
    let db = db();
    let dep = bodies(&db, krate(&db, "dep"));
    let &(_, with) = dep.iter().find(|(name, _)| name == "with").expect("`S::with`");

    Body::of(&db, with);
    let tree = body_syntax(&db, with).as_ref().expect("a body written in a file").tree(&db);
    assert!(tree.lazies().count() >= 2, "the anchor of `S::with` is the `impl`, with both bodies");
    let entered = parsed(&tree);
    assert_eq!(entered.len(), 1, "{entered:?}");
    assert!(entered[0].contains("let z"), "{entered:?}");

    // The file's own tree: the item tree reads the inner attributes of `attributed`, and what
    // reads the attributes of `S::with` may enter its body; nothing else is parsed.
    let file = file_tree(&db, with);
    for body in parsed(file) {
        assert!(body.contains("let z") || body.contains("#![allow(unused)]"), "{body:?}");
    }
}

/// The text of each lazy node of `tree` that was parsed.
fn parsed(tree: &FlatTree) -> Vec<&str> {
    tree.lazies().filter(|&at| tree.is_parsed(at)).map(|at| &tree.source()[tree.range(at)]).collect()
}
