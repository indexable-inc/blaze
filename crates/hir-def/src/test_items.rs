//! A fixture's items found by name, for the tests of the crates above hir-def: the lookups each
//! of their test modules wrote again.

use base_db::{Crate, Db};

use crate::{
    ConstId, FunctionId, ImplId, ModuleDefId, StaticId, TypeAliasId,
    nameres::crate_def_map,
    signatures::{ConstSignature, FunctionSignature, StaticSignature, TypeAliasSignature},
};

/// Every item declared in `krate`'s modules, module by module, each in the order written.
pub fn declarations(db: &Db, krate: Crate) -> Vec<ModuleDefId> {
    // alloc: a test helper's list, what its callers index and count.
    crate_def_map(db, krate).modules(db).flat_map(|(_, module)| module.scope(db).declarations()).collect()
}

/// Every impl in `krate`'s modules, module by module, each in the order written.
pub fn impls(db: &Db, krate: Crate) -> Vec<ImplId> {
    // alloc: a test helper's list, what its callers index and count.
    crate_def_map(db, krate).modules(db).flat_map(|(_, module)| module.scope(db).impls()).collect()
}

/// The one function named `name` in `krate`; panics with how many there are otherwise.
#[track_caller]
pub fn function(db: &Db, krate: Crate, name: &str) -> FunctionId {
    one(db, krate, "functions", name, |def| match def {
        ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => Some(it),
        _ => None,
    })
}

/// The one type alias named `name` in `krate`; panics with how many there are otherwise.
#[track_caller]
pub fn type_alias(db: &Db, krate: Crate, name: &str) -> TypeAliasId {
    one(db, krate, "type aliases", name, |def| match def {
        ModuleDefId::TypeAliasId(it) if TypeAliasSignature::of(db, it).name.as_str() == name => Some(it),
        _ => None,
    })
}

/// The one static named `name` in `krate`; panics with how many there are otherwise.
#[track_caller]
pub fn static_item(db: &Db, krate: Crate, name: &str) -> StaticId {
    one(db, krate, "statics", name, |def| match def {
        ModuleDefId::StaticId(it) if StaticSignature::of(db, it).name.as_str() == name => Some(it),
        _ => None,
    })
}

/// The one named constant `name` in `krate`; panics with how many there are otherwise.
#[track_caller]
pub fn constant(db: &Db, krate: Crate, name: &str) -> ConstId {
    one(db, krate, "constants", name, |def| match def {
        ModuleDefId::ConstId(it)
            if ConstSignature::of(db, it).name.as_ref().is_some_and(|it| it.as_str() == name) =>
        {
            Some(it)
        }
        _ => None,
    })
}

#[track_caller]
fn one<T: Copy>(db: &Db, krate: Crate, what: &str, name: &str, pick: impl FnMut(ModuleDefId) -> Option<T>) -> T {
    // alloc: a test helper; the count is in the panic.
    let found: Vec<T> = declarations(db, krate).into_iter().filter_map(pick).collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} {what} named `{name}`", found.len()),
    }
}
