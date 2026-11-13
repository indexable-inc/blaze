//! What a crate keeps by name ([`kept`]): rustc's `linked_symbols` of it, which every program
//! that links the crate keeps though no code of it names them. A function with a C-level
//! symbol (`#[no_mangle]`, `#[export_name]`) or one of the standard library's linked by name
//! (`#[rustc_std_internal_symbol]`, `#[panic_handler]`, the personality); a static that is
//! `#[used]` or exported; either with a `#[linkage]` other than `internal`. Items declared in
//! bodies count, as rustc's collector finds them there: `const _: () = { #[used] static .. };`
//! is how a constructor is registered.
//!
//! What is found at module and impl level is read from the crate's def map, which lowers no
//! body. What is declared in bodies is found by lowering only the bodies that can declare it:
//! for a crate served from its records, the bodies its kept-roots record names ([`frozen`],
//! the bodies whose blocks declared a kept item when the record was written); for any other,
//! the bodies whose text can say a root (`hir_def::nameres::demand::bodies_declaring_roots`,
//! whose filter is by the words an attribute that keeps an item is spelled with). No other
//! body is lowered: before this, every body of every crate a build linked was, the standard
//! library's included.

pub mod frozen;
#[cfg(test)]
mod tests;

use base_db::{Crate, Db};
use hir_def::{
    AssocItemId, AttrDefId, DefWithBodyId, FunctionId, ModuleDefId, StaticId,
    attrs::AttrFlags,
    nameres::{
        DefMap, crate_def_map,
        demand::{bodies_declaring_roots, body_blocks},
    },
};

use crate::{exported_static, exported_symbol, linked_symbol};

/// The functions and statics of a crate that are kept by name, in the order found: those at
/// module and impl level first, then those of each of `owners` in turn. `owners` are the
/// bodies whose blocks declare one of them, in the order found: what a kept-roots record
/// names ([`frozen`]).
#[derive(PartialEq, Eq, Debug, Default)]
pub(super) struct Kept {
    pub(super) functions: Box<[FunctionId]>,
    pub(super) statics: Box<[StaticId]>,
    pub(super) owners: Box<[DefWithBodyId]>,
}

/// The functions `krate` keeps by name ([`kept`]), each as the instance a build roots
/// ([`root_in`](crate::v2::frontend::root_in)), in the order found: once per crate and
/// revision of it, so a build that lists its roots asks one memo a crate it links. Asked
/// function by function, each root's generics were read again every build: 352
/// `FunctionSignature::kept` verified for a one-line body edit of hello world (2026-11-12),
/// a count of the names the program's crates keep.
#[blaze_query::tracked(returns(ref))]
pub(super) fn kept_roots(db: &Db, krate: Crate) -> Box<[crate::v2::HirInstance]> {
    let functions = kept(db, krate).functions.iter();
    // alloc: once a crate a revision, the memo's list
    functions.map(|func| crate::v2::frontend::root_in(db, *func)).collect()
}

/// Once per crate and revision of it: the standard library's is found once a daemon, and
/// read from its record when it is served.
#[blaze_query::tracked(returns(ref))]
pub(super) fn kept(db: &Db, krate: Crate) -> Kept {
    match frozen::served(db, krate) {
        Some(listed) => from_record(db, listed),
        None => over_bodies(db, krate, &bodies_declaring_roots(db, krate)),
    }
}

/// What a crate keeps, its record read: the record's lists, then what the blocks of each body
/// it names declare. Lowers those bodies alone.
fn from_record(db: &Db, listed: frozen::Listed) -> Kept {
    let frozen::Listed { mut functions, mut statics, owners } = listed;
    for &owner in &owners {
        in_blocks(db, owner, &mut functions, &mut statics);
    }
    Kept { functions: functions.into(), statics: statics.into(), owners: owners.into() }
}

/// What `krate` keeps, found at module and impl level and in the blocks of `bodies` (in their
/// order): lowers `bodies` alone. A body whose blocks declare a kept item is one of the
/// result's owners.
pub(super) fn over_bodies(db: &Db, krate: Crate, bodies: &[DefWithBodyId]) -> Kept {
    // alloc: once a crate a revision, the memo's lists
    let (mut functions, mut statics) = (Vec::new(), Vec::new());
    in_def_map(db, crate_def_map(db, krate), &mut functions, &mut statics);
    // alloc: as above; few, the bodies that declare a kept item
    let mut owners = Vec::new();
    for &owner in bodies {
        if in_blocks(db, owner, &mut functions, &mut statics) {
            owners.push(owner);
        }
    }
    Kept { functions: functions.into(), statics: statics.into(), owners: owners.into() }
}

/// What `krate` keeps at module and impl level, from its def map alone: the lists a
/// kept-roots record holds.
pub(super) fn at_module_level(db: &Db, krate: Crate) -> (Vec<FunctionId>, Vec<StaticId>) {
    // alloc: once a crate per record written
    let (mut functions, mut statics) = (Vec::new(), Vec::new());
    in_def_map(db, crate_def_map(db, krate), &mut functions, &mut statics);
    (functions, statics)
}

/// Adds what the blocks of the body of `owner` declare that is kept, nested blocks included;
/// whether there was any. An item with a body declared in one is an owner of its own: its
/// blocks are not looked in here.
fn in_blocks(
    db: &Db,
    owner: DefWithBodyId,
    functions: &mut Vec<FunctionId>,
    statics: &mut Vec<StaticId>,
) -> bool {
    let (had_functions, had_statics) = (functions.len(), statics.len());
    for &(_, def_map) in &body_blocks(db, owner).blocks {
        in_def_map(db, def_map, functions, statics);
    }
    functions.len() > had_functions || statics.len() > had_statics
}

/// Adds what the modules of `def_map` declare that is kept, and what their impls do.
fn in_def_map(
    db: &Db,
    def_map: &DefMap,
    functions: &mut Vec<FunctionId>,
    statics: &mut Vec<StaticId>,
) {
    for (_, data) in def_map.modules(db) {
        let scope = data.scope(db);
        for def in scope.declarations() {
            match def {
                ModuleDefId::FunctionId(func) if keeps_function(db, func) => functions.push(func),
                ModuleDefId::StaticId(value) if keeps_static(db, value) => statics.push(value),
                _ => {}
            }
        }
        for imp in scope.impls() {
            for &(_, item) in imp.impl_items(db).items.iter() {
                if let AssocItemId::FunctionId(func) = item
                    && keeps_function(db, func)
                {
                    functions.push(func);
                }
            }
        }
    }
}

/// A generic one is left out, as rustc leaves it (`no_mangle_generic_items`).
fn keeps_function(db: &Db, func: FunctionId) -> bool {
    super::compiled(db, func)
        && (exported_symbol(db, func).or_else(|| linked_symbol(db, func)).is_some()
            || custom_linkage(db, func.into()))
}

fn keeps_static(db: &Db, value: StaticId) -> bool {
    exported_static(db, value).is_some()
        || AttrFlags::query(db, value.into()).contains(AttrFlags::USED)
        || custom_linkage(db, value.into())
}

/// rustc's `contains_extern_indicator` of `#[linkage = ..]` (feature `linkage`): any linkage but
/// `internal`.
fn custom_linkage(db: &Db, owner: AttrDefId) -> bool {
    AttrFlags::linkage(db, owner).is_some_and(|it| it.as_str() != "internal")
}
