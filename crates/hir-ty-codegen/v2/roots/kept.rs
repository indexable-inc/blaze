//! What a crate keeps by name ([`kept`]): rustc's `linked_symbols` of it, which every program
//! that links the crate keeps though no code of it names them. A function with a C-level
//! symbol (`#[no_mangle]`, `#[export_name]`) or one of the standard library's linked by name
//! (`#[rustc_std_internal_symbol]`, `#[panic_handler]`, the personality); a static that is
//! `#[used]` or exported; either with a `#[linkage]` other than `internal`. Items declared in bodies count, as rustc's collector finds them
//! there: `const _: () = { #[used] static .. };` is how a constructor is registered.

use base_db::{Crate, Db};
use hir_def::{
    AssocItemId, AttrDefId, DefWithBodyId, ExpressionStoreOwnerId, FunctionId, ModuleDefId,
    StaticId,
    attrs::AttrFlags,
    expr_store::ExpressionStore,
    nameres::{DefMap, crate_def_map},
    signatures::{FunctionSignature, StaticFlags, StaticSignature},
};

use crate::{exported_static, exported_symbol, linked_symbol};

/// The functions and statics of a crate that are kept by name, in the order found.
#[derive(PartialEq, Eq, Debug, Default)]
pub(super) struct Kept {
    pub(super) functions: Box<[FunctionId]>,
    pub(super) statics: Box<[StaticId]>,
}

/// Once per crate and revision of it: the standard library's is found once a daemon.
#[blaze_query::tracked(returns(ref))]
pub(super) fn kept(db: &Db, krate: Crate) -> Kept {
    // alloc: once a crate a revision, the memo's lists and the walk's two stacks
    let (mut functions, mut statics) = (Vec::new(), Vec::new());
    // alloc: as above
    let mut bodies = Vec::new();
    // alloc: as above
    let mut maps: Vec<&DefMap> = vec![crate_def_map(db, krate)];
    while let Some(def_map) = maps.pop() {
        for (_, data) in def_map.modules(db) {
            let scope = data.scope(db);
            for def in scope.declarations() {
                match def {
                    ModuleDefId::FunctionId(func) => {
                        if FunctionSignature::of(db, func).has_body() {
                            bodies.push(DefWithBodyId::FunctionId(func));
                        }
                        if keeps_function(db, func) {
                            functions.push(func);
                        }
                    }
                    ModuleDefId::StaticId(value) => {
                        if !StaticSignature::of(db, value).flags.contains(StaticFlags::EXTERN) {
                            bodies.push(DefWithBodyId::StaticId(value));
                        }
                        if keeps_static(db, value) {
                            statics.push(value);
                        }
                    }
                    ModuleDefId::ConstId(it) => bodies.push(DefWithBodyId::ConstId(it)),
                    ModuleDefId::TraitId(it) => {
                        for &(_, item) in it.trait_items(db).items.iter() {
                            assoc_body(db, item, &mut bodies);
                        }
                    }
                    _ => {}
                }
            }
            for it in scope.unnamed_consts() {
                bodies.push(DefWithBodyId::ConstId(it));
            }
            for imp in scope.impls() {
                for &(_, item) in imp.impl_items(db).items.iter() {
                    if let AssocItemId::FunctionId(func) = item
                        && keeps_function(db, func)
                    {
                        functions.push(func);
                    }
                    assoc_body(db, item, &mut bodies);
                }
            }
        }
        for owner in bodies.drain(..) {
            let store = ExpressionStore::of(db, ExpressionStoreOwnerId::Body(owner));
            maps.extend(store.blocks(db).map(|(_, map)| map));
        }
    }
    Kept { functions: functions.into(), statics: statics.into() }
}

fn assoc_body(db: &Db, item: AssocItemId, bodies: &mut Vec<DefWithBodyId>) {
    match item {
        AssocItemId::FunctionId(it) if FunctionSignature::of(db, it).has_body() => {
            bodies.push(DefWithBodyId::FunctionId(it))
        }
        AssocItemId::FunctionId(_) => {}
        AssocItemId::ConstId(it) => bodies.push(DefWithBodyId::ConstId(it)),
        AssocItemId::TypeAliasId(_) => {}
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
