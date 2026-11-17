//! Tests over fixtures: `hir_expand::fixture` builds [`db::TestDB`] from text.

mod bound;
mod coherence;
mod db;
mod facts;
mod form;
mod goals;
mod hashes;
mod mem;
mod narrow;
mod own_tables;
mod table;

use std::alloc::Global;

use blaze_memo::testing::Ran;
use hir_def::{ModuleDefId, ModuleId, nameres::crate_def_map};
use hir_expand::fixture;

use crate::{ItemId, key};

pub(crate) use db::TestDB;

/// Every item of the fixture's own crate outside bodies: of its modules, impls and traits.
pub(crate) fn items(db: &TestDB) -> Vec<ItemId> {
    let krate = fixture::test_crate(db);
    let mut items = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        let scope = module.scope(db);
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(it) => items.push(ItemId::Function(it)),
                ModuleDefId::AdtId(it) => items.push(ItemId::Adt(it)),
                ModuleDefId::ConstId(it) => items.push(ItemId::Const(it)),
                ModuleDefId::StaticId(it) => items.push(ItemId::Static(it)),
                ModuleDefId::TraitId(it) => {
                    items.push(ItemId::Trait(it));
                    items.extend(it.trait_items(db).items.iter().map(|&(_, it)| ItemId::from(it)));
                }
                ModuleDefId::TypeAliasId(it) => items.push(ItemId::Alias(it)),
                ModuleDefId::MacroId(it) => items.push(ItemId::Macro(it)),
                ModuleDefId::EnumVariantId(_)
                | ModuleDefId::ModuleId(_)
                | ModuleDefId::BuiltinType(_) => {}
            }
        }
        for it in scope.impls() {
            items.push(ItemId::Impl(it));
            items.extend(it.impl_items(db).items.iter().map(|&(_, it)| ItemId::from(it)));
        }
        items.extend(scope.unnamed_consts().map(ItemId::Const));
    }
    items
}

/// The item at `path` in the fixture's crate, its crate's name left out (`m::f`, `S`).
#[track_caller]
pub(crate) fn item(db: &TestDB, path: &str) -> ItemId {
    let found: Vec<ItemId> = items(db)
        .into_iter()
        .filter(|&id| {
            let full = key::path(db, id);
            full.split_once("::").map(|(_, rest)| rest) == Some(path)
        })
        .collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} items at `{path}`: {found:?}", found.len()),
    }
}

/// The module of the fixture's crate named `name`.
#[track_caller]
pub(crate) fn module(db: &TestDB, name: &str) -> ModuleId {
    crate_def_map(db, fixture::test_crate(db))
        .modules(db)
        .map(|(id, _)| id)
        .find(|id| id.name(db).is_some_and(|it| it.as_str() == name))
        .unwrap_or_else(|| panic!("no module `{name}`"))
}

/// The database of a one-file fixture, and a way to set that file's text.
pub(crate) fn single(text: &str) -> (TestDB, base_db::FileId) {
    let fx = fixture::db(text);
    let file = fx.only_file();
    let db = fx.into_db();
    let file = file.file_id(&db);
    (db, file)
}

/// Sets `file`'s text in a new snapshot: one edit and its commit. A file's id and a crate's key
/// are the same across it; every other id (an item's, a [`base_db::Crate`]) is of the snapshot
/// it was taken in, taken again after.
pub(crate) fn edit(db: &mut TestDB, file: base_db::FileId, text: &str) {
    let mut edit = db.edit();
    edit.set_file_text(file, text);
    _ = edit.commit();
}

/// What `work` ran, from the database's run log (a fixture's database logs its runs): the runs
/// before it are taken off first, so none of them is counted as its.
pub(crate) fn runs_of(db: &TestDB, work: impl FnOnce()) -> Vec<Ran, Global> {
    let log = db.runs().expect("a fixture's database logs its runs");
    _ = log.take();
    work();
    log.take()
}

/// How many of `runs` ran the query of the kind named `kind` (its generated `KIND_NAME`),
/// whatever its key.
pub(crate) fn count(runs: &[Ran], kind: &str) -> usize {
    runs.iter().filter(|it| it.query == kind).count()
}

/// Whether any of `runs` ran the query of the kind named `kind` (its `KIND_NAME`).
pub(crate) fn ran(runs: &[Ran], kind: &str) -> bool {
    count(runs, kind) > 0
}
