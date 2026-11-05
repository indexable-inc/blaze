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
mod table;

use hir_def::{ModuleDefId, nameres::crate_def_map};
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

/// The database of a one-file fixture, and a way to set that file's text.
pub(crate) fn single(text: &str) -> (TestDB, base_db::FileId) {
    let fx = fixture::db(text);
    let file = fx.only_file();
    let db = fx.into_db();
    let file = file.file_id(&db);
    (db, file)
}

pub(crate) fn edit(db: &mut TestDB, file: base_db::FileId, text: &str) {
    db.set_file_text(file, text);
}
