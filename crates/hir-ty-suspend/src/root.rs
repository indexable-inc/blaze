//! The roots: the calls that park a fiber. corosensei's `Yielder::suspend` switches a fiber out,
//! and only `blaze_graph::sched::fiber` (the fiber driver) calls it, as the lint
//! `blaze::fiber_boundary` holds it; everything that reaches it is may-suspend.

use base_db::{Crate, CrateOrigin, Db, LangCrateOrigin};
use blaze_hash::HashSet;
use hir_def::{
    AssocItemId, FunctionId, ModuleDefId, ModuleId, TraitId,
    nameres::{ModuleData, crate_def_map},
};
use hir_expand::name::Name;
use hir_ty::{
    method_resolution::InherentImpls,
    next_solver::{SimplifiedType, SolverDefId},
};
use intern::Symbol;

/// The crate whose function parks a fiber, by its crate name.
const FIBERS: &str = "corosensei";

/// The roots of `krate`'s crate graph: `Yielder::suspend` of each `corosensei` the graph has
/// (one per version). Empty when it has none, and then nothing is walked.
#[blaze_query::tracked(returns(deref))]
pub(crate) fn roots(db: &Db, krate: Crate) -> Box<[FunctionId]> {
    let mut found = Vec::new(); // alloc: the roots, once per crate graph, one per corosensei
    let mut seen = HashSet::default();
    let mut stack = vec![krate]; // alloc: the walk of the crate graph, once per crate
    while let Some(at) = stack.pop() {
        if !seen.insert(at) {
            continue;
        }
        if is_named(db, at, FIBERS)
            && let Some(suspend) = inherent_method(db, at, "Yielder", "suspend")
        {
            found.push(suspend);
        }
        stack.extend(at.data(db).dependencies.iter().map(|dep| dep.crate_id));
    }
    found.into_boxed_slice()
}

fn is_named(db: &Db, krate: Crate, name: &str) -> bool {
    krate.extra_data(db).display_name.as_ref().is_some_and(|it| it.crate_name().as_str() == name)
}

/// The inherent method `method` of the type `ty` at the root of `krate`.
fn inherent_method(db: &Db, krate: Crate, ty: &str, method: &str) -> Option<FunctionId> {
    let root = crate_def_map(db, krate).root_module_id();
    let Some(ModuleDefId::AdtId(adt)) = data(db, root).name_in_scope(db, &name_of(ty)).take_types() else {
        return None;
    };
    let name = Symbol::intern(method);
    let self_ty = SimplifiedType::Adt(SolverDefId::AdtId(adt));
    InherentImpls::for_crate(db, krate).for_self_ty(&self_ty).iter().find_map(|&imp| {
        imp.impl_items(db).items.iter().find_map(|(it, item)| match *item {
            AssocItemId::FunctionId(func) if *it.symbol() == name => Some(func),
            _ => None,
        })
    })
}

/// `core::marker::Send` of `krate`'s graph.
#[blaze_query::tracked]
pub(crate) fn send_trait(db: &Db, krate: Crate) -> Option<TraitId> {
    let core = home_crate(db, krate, LangCrateOrigin::Core)?;
    let root = crate_def_map(db, core).root_module_id();
    let Some(ModuleDefId::ModuleId(marker)) = data(db, root).name_in_scope(db, &name_of("marker")).take_types()
    else {
        return None;
    };
    match data(db, marker).name_in_scope(db, &name_of("Send")).take_types()? {
        ModuleDefId::TraitId(it) => Some(it),
        _ => None,
    }
}

/// `krate` itself when it is `lang`, else the dependency of it that is.
fn home_crate(db: &Db, krate: Crate, lang: LangCrateOrigin) -> Option<Crate> {
    let is = |krate: Crate| matches!(krate.data(db).origin, CrateOrigin::Lang(it) if it == lang);
    if is(krate) {
        return Some(krate);
    }
    krate.data(db).dependencies.iter().map(|dep| dep.crate_id).find(|&dep| is(dep))
}

fn data(db: &Db, module: ModuleId) -> &ModuleData {
    module.to_db(db).def_map(db).module(db, module)
}

fn name_of(segment: &str) -> Name {
    Name::new_symbol_root(Symbol::intern(segment))
}
