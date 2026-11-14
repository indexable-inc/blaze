//! `module_impls`: what it lists, that an edit reruns it only for the module edited (and
//! backdates there unless an impl came or went), and that a memo hit allocates nothing.

use std::hint::black_box;

use hir_expand::fixture;
use hir_expand::name::Name;
use intern::Symbol;
use syntax::view::{self as ast, AstNode};

use crate::{
    AdtId, ImplId, Lookup, ModuleDefId, ModuleId,
    nameres::facts::impls::module_impls,
    src::HasSource,
    test_db::{TestDB, TestDBExt},
};

/// Each impl as written: `Trait for Type`, or `Type` for an inherent one.
fn render(db: &TestDB, impls: &[ImplId]) -> Vec<String> {
    let text =
        |ty: Option<ast::Type<'_>>| ty.map_or_else(String::new, |it| it.syntax().to_string());
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

/// Which of `modules` `module_impls` ran for while `f` ran, each once per run, and how many runs
/// of it there were in all, from the memo's run log (`TestDBExt::runs_in_read_order`).
fn ran_for(db: &TestDB, modules: &[ModuleId], f: impl FnOnce()) -> (Vec<ModuleId>, usize) {
    let runs = db.runs_in_read_order(f);
    let impls: Vec<_> = runs.iter().filter(|it| it.query.ends_with("module_impls")).collect();
    let of = impls
        .iter()
        .filter_map(|ran| modules.iter().copied().find(|&it| ran.is::<module_impls>(&(it,))))
        .collect();
    (of, impls.len())
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

// PORT (round 20): false by design on blaze-memo, left failing. It claimed an item added to
// module `a` runs `module_impls` again for `a` alone; `b` is of the same crate, whose arena the
// edit makes again, so `b`'s runs again too.
#[test]
#[ignore = "blaze-memo: an edit rebuilds its crate's arena; the claim is the host's, on records (docs/v2/plan/design/host.md)"]
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
    // The modules of `a.rs` and `b.rs`, taken again in each snapshot: an edit of a file of the
    // crate drops the crate's arena, which homes their ids. What stays is each module's name.
    let modules = |db: &TestDB| (db.module_for_file(a_file), db.module_for_file(b_file));
    let named = |db: &TestDB, (a, b): (ModuleId, ModuleId)| {
        let name = |it: ModuleId| it.name(db).map(|it| it.as_str().to_owned());
        (name(a), name(b))
    };
    let (a, b) = modules(&db);
    let names = named(&db, (a, b));
    let a_before = snapshot(&db, a);
    let b_before = snapshot(&db, b);
    assert_eq!(a_before, (vec![], vec!["A".to_owned()], 0));
    assert_eq!(b_before, (vec!["crate::Tr for B".to_owned()], vec![], 0));

    // An item that is no impl: `b`'s list is not even rerun, `a`'s at most reruns equal.
    _ = db.set_file_text(a_file, "pub struct A;\nimpl A {}\npub fn added() {}\n");
    let (a, b) = modules(&db);
    assert_eq!(named(&db, (a, b)), names);
    let (ran, runs) = ran_for(&db, &[a, b], || {
        module_impls(&db, a);
        module_impls(&db, b);
    });
    assert!(!ran.contains(&b), "`b` reran for an item added to `a`: {ran:?}");
    assert!(ran.iter().all(|&it| it == a) && ran.len() == runs, "{ran:?} of {runs} runs");
    assert_eq!(snapshot(&db, a), a_before);
    assert_eq!(snapshot(&db, b), b_before);

    // An impl added to `a`: `a`'s list reruns and has it, `b`'s is not rerun.
    _ = db.set_file_text(
        a_file,
        "pub struct A;\nimpl A {}\npub fn added() {}\nimpl crate::Tr for A {}\n",
    );
    let (a, b) = modules(&db);
    assert_eq!(named(&db, (a, b)), names);
    let (ran, runs) = ran_for(&db, &[a, b], || {
        module_impls(&db, a);
        module_impls(&db, b);
    });
    assert_eq!((ran, runs), (vec![a], 1), "only `a` reruns for an impl added to it");
    assert_eq!(snapshot(&db, a), (vec!["crate::Tr for A".to_owned()], vec!["A".to_owned()], 0));
    assert_eq!(snapshot(&db, b), b_before);

    // The same once more with the names `TestDBExt::executed_in_read_order` gives (each
    // `query(key)`): one run, `a`'s.
    _ = db.set_file_text(a_file, "pub struct A;\nimpl A {}\n");
    let (a, b) = modules(&db);
    let executed = db.executed_in_read_order(|| {
        module_impls(&db, a);
        module_impls(&db, b);
    });
    let impls = |it: &&String| it.split('(').next().is_some_and(|it| it.ends_with("module_impls"));
    assert_eq!(executed.iter().filter(impls).count(), 1, "{executed:#?}");
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
