//! `module_impls`: what it lists, that an edit reruns it only for the module edited (and
//! backdates there unless an impl came or went), and that a memo hit allocates nothing.

use std::hint::black_box;

use base_db::Db;
use hir_expand::name::Name;
use intern::Symbol;
use blaze_query::db::AsId;
use syntax::view::{self as ast, AstNode};
use hir_expand::fixture;

use crate::{
    AdtId, ImplId, Lookup, ModuleDefId, ModuleId, nameres::facts::impls::module_impls,
    src::HasSource, test_db::{TestDB, TestDBExt},
};

/// Each impl as written: `Trait for Type`, or `Type` for an inherent one.
fn render(db: &TestDB, impls: &[ImplId]) -> Vec<String> {
    let text = |ty: Option<ast::Type<'_>>| ty.map_or_else(String::new, |it| it.syntax().to_string());
    impls
        .iter()
        .map(|&it| {
            let source = it.lookup(db).source_view(db).value;
            match source.trait_() {
                Some(trait_) => format!("{} for {}", text(Some(trait_)), text(source.self_ty())),
                None => text(source.self_ty()),
            }
        })
        .collect()
}

/// What `module_impls` of `module` holds, as text.
fn snapshot(db: &TestDB, module: ModuleId) -> (Vec<String>, Vec<String>, usize) {
    let impls = module_impls(db, module);
    (
        render(db, &impls.trait_impls),
        render(db, &impls.inherent_impls),
        impls.builtin_derive_impls.len(),
    )
}

/// The modules `module_impls` ran for while `f` ran, by key, from the same `WillExecute` events
/// `TestDB::log_executed` names.
fn ran_for(db: &TestDB, f: impl FnOnce()) -> Vec<blaze_query::db::Id> {
    let base: &Db = db;
    db.log_in_read_order(f)
        .into_iter()
        .filter_map(|event| match event.kind {
            blaze_query::db::EventKind::WillExecute { database_key } => {
                let name = blaze_query::db::Database::storage(base)
                    .ingredient_name(database_key.ingredient_index());
                (name == "module_impls").then(|| database_key.key_index())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn lists_a_modules_impls_in_order() {
    let fx = fixture::db(
        r#"
//- /main.rs crate:main deps:core
use core::{Clone, Copy, derive};

mod inner;

pub trait Tr {}
pub trait Other<T> {}
pub struct S;
pub struct U;

impl S {}
impl Tr for S {}
impl<T> Tr for (T,) {}
impl U {}
impl Other<U> for S {}

#[derive(Copy, Clone)]
pub struct D;

#[cfg(any())]
impl Tr for U {}

macro_rules! make {
    () => { impl Tr for U {} };
}
make!();

//- /inner.rs
impl crate::S {}

//- /core.rs crate:core
#[rustc_builtin_macro]
pub macro derive($item:item) {}
#[rustc_builtin_macro]
pub macro Copy {}
#[rustc_builtin_macro]
pub macro Clone {}
"#,
    );
    let files = fx.files().to_vec();
    let db = fx.into_db();
    let main = db.module_for_file(files[0].file_id(&db));
    let impls = module_impls(&db, main);
    assert_eq!(
        render(&db, &impls.trait_impls),
        ["Tr for S", "Tr for (T,)", "Other<U> for S", "Tr for U"],
    );
    assert_eq!(render(&db, &impls.inherent_impls), ["S", "U"]);

    let d = match main.def_map(&db).module(&db, main).scope(&db).get(&name("D")).take_types() {
        Some(ModuleDefId::AdtId(it @ AdtId::StructId(_))) => it,
        other => panic!("`D` is {other:?}"),
    };
    let mut derived: Vec<_> = impls
        .builtin_derive_impls
        .iter()
        .map(|it| {
            let loc = it.loc(&db);
            assert_eq!(loc.adt, d);
            loc.trait_.name().as_str().to_owned()
        })
        .collect();
    derived.sort();
    assert_eq!(derived, ["Clone", "Copy"]);

    let inner = db.module_for_file(files[1].file_id(&db));
    assert_eq!(snapshot(&db, inner), (vec![], vec!["crate::S".to_owned()], 0));
}

#[test]
fn reruns_only_for_the_module_edited() {
    let fx = fixture::db(
        r#"
//- /main.rs crate:main
mod a;
mod b;

pub trait Tr {}

//- /a.rs
pub struct A;
impl A {}

//- /b.rs
pub struct B;
impl crate::Tr for B {}
"#,
    );
    let files = fx.files().to_vec();
    let mut db = fx.into_db();
    let (a_file, b_file) = (files[1].file_id(&db), files[2].file_id(&db));
    let (a, b) = (db.module_for_file(a_file), db.module_for_file(b_file));
    let a_before = snapshot(&db, a);
    let b_before = snapshot(&db, b);
    assert_eq!(a_before, (vec![], vec!["A".to_owned()], 0));
    assert_eq!(b_before, (vec!["crate::Tr for B".to_owned()], vec![], 0));

    // An item that is no impl: `b`'s list is not even rerun, `a`'s at most reruns equal.
    db.set_file_text(a_file, "pub struct A;\nimpl A {}\npub fn added() {}\n");
    assert_eq!((db.module_for_file(a_file), db.module_for_file(b_file)), (a, b));
    let ran = ran_for(&db, || {
        module_impls(&db, a);
        module_impls(&db, b);
    });
    assert!(!ran.contains(&b.as_id()), "`b` reran for an item added to `a`: {ran:?}");
    assert!(ran.iter().all(|&it| it == a.as_id()), "{ran:?}");
    assert_eq!(snapshot(&db, a), a_before);
    assert_eq!(snapshot(&db, b), b_before);

    // An impl added to `a`: `a`'s list reruns and has it, `b`'s is not rerun.
    db.set_file_text(a_file, "pub struct A;\nimpl A {}\npub fn added() {}\nimpl crate::Tr for A {}\n");
    assert_eq!((db.module_for_file(a_file), db.module_for_file(b_file)), (a, b));
    let ran = ran_for(&db, || {
        module_impls(&db, a);
        module_impls(&db, b);
    });
    assert_eq!(ran, [a.as_id()], "only `a` reruns for an impl added to it");
    assert_eq!(snapshot(&db, a), (vec!["crate::Tr for A".to_owned()], vec!["A".to_owned()], 0));
    assert_eq!(snapshot(&db, b), b_before);

    // The same once more with the names `TestDB::log_executed` gives: one run, `a`'s.
    db.set_file_text(a_file, "pub struct A;\nimpl A {}\n");
    let executed = db.executed_in_read_order(|| {
        module_impls(&db, a);
        module_impls(&db, b);
    });
    assert_eq!(executed.iter().filter(|it| *it == "module_impls").count(), 1, "{executed:#?}");
    assert_eq!(snapshot(&db, a), a_before);
}

/// Allocation budget: a memo hit of `module_impls` and reading what it holds allocate nothing.
#[test]
fn a_memo_hit_allocates_nothing() {
    let fx = fixture::db(
        r#"
//- /main.rs crate:main
pub trait Tr {}
pub struct S;
impl S {}
impl Tr for S {}
impl Tr for () {}
"#,
    );
    let files = fx.files().to_vec();
    let db = fx.into_db();
    let main = db.module_for_file(files[0].file_id(&db));
    let warm = module_impls(&db, main);
    assert_eq!((warm.trait_impls.len(), warm.inherent_impls.len()), (2, 1), "too small to measure");
    blaze_alloc_count::assert_at_most(0, "a memo hit of `module_impls`", || {
        let impls = module_impls(&db, main);
        for &it in impls.trait_impls.iter().chain(impls.inherent_impls.iter()) {
            black_box(it);
        }
        black_box(impls.builtin_derive_impls.len());
    });
}

fn name(text: &str) -> Name {
    Name::new_symbol_root(Symbol::intern(text))
}
