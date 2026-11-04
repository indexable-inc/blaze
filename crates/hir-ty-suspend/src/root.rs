//! The roots: the calls that park a fiber. `blaze_graph`'s stack layer,
//! `sched::stack::Yielder::suspend`, switches a fiber out on every target (corosensei's on
//! native, the seed host's on wasm, chosen in that one module), and only the fiber drivers call it;
//! everything that reaches it is may-suspend.

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
use std::{alloc::Global, fmt};

/// The crate whose function parks a fiber, by its crate name.
const FIBERS: &str = "blaze_graph";
/// The modules from [`FIBERS`]'s root to the stack layer.
const LAYER: [&str; 2] = ["sched", "stack"];
/// The type and the inherent method of it, in [`LAYER`], that parks a fiber.
const ROOT: (&str, &str) = ("Yielder", "suspend");

/// A crate graph's roots, or why one is missing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Roots {
    /// `Yielder::suspend` of each `blaze_graph` the graph has (one per version); empty when it
    /// has none, and then nothing is walked.
    pub(crate) found: Box<[FunctionId]>,
    /// A `blaze_graph` whose stack layer has no `Yielder::suspend`: an error, never an empty walk.
    pub(crate) missing: Option<MissingRoot>,
}

/// A `blaze_graph` in the crate graph without the function that parks a fiber, so the check
/// has no root to walk from: its stack layer moved or renamed it, and [`FIBERS`], [`LAYER`] and
/// [`ROOT`] must follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MissingRoot {
    /// The `blaze_graph` without it.
    pub krate: Crate,
    /// The first name of `sched::stack::Yielder::suspend` it does not have.
    pub missing: &'static str,
}

impl fmt::Display for MissingRoot {
    /// The error without which `blaze_graph` it is; [`MissingRoot::said`] names that too.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the crate graph has `{FIBERS}` but no `{FIBERS}::{}::{}::{}` (no `{}` there), the function that parks a fiber: the may-suspend check has no root to walk from (hir-ty-suspend's root.rs names it)",
            LAYER.join("::"),
            ROOT.0,
            ROOT.1,
            self.missing
        )
    }
}

impl MissingRoot {
    /// The error naming which `blaze_graph` lacks the root, for a graph with more than one: its
    /// version, its root file and what it is built for.
    pub fn said(&self, db: &Db) -> String {
        let data = self.krate.data(db);
        let root = db.path_for_file(data.root_file_id);
        // alloc: an error's message, once
        let root = root.map_or_else(|| format!("{:?}", data.root_file_id), |it| it.to_string());
        let extra = self.krate.extra_data(db);
        let version = extra.version.as_deref().unwrap_or("of no version");
        // alloc: as above
        format!("{self}; that `{FIBERS}` is {version} at `{root}`, built for {}", extra.built_for)
    }
}

/// The roots of `krate`'s crate graph: the stack layer's `Yielder::suspend` of each
/// `blaze_graph` the graph has.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn roots(db: &Db, krate: Crate) -> Roots {
    let mut found = Vec::new(); // alloc: the roots, once per crate graph, one per blaze_graph
    let mut seen = HashSet::new_in(Global);
    let mut stack = vec![krate]; // alloc: the walk of the crate graph, once per crate
    while let Some(at) = stack.pop() {
        if !seen.insert(at) {
            continue;
        }
        if is_named(db, at, FIBERS) {
            match suspend_of(db, at) {
                Ok(suspend) => found.push(suspend),
                Err(missing) => {
                    return Roots {
                        found: Box::default(),
                        missing: Some(MissingRoot { krate: at, missing }),
                    };
                }
            }
        }
        stack.extend(at.data(db).dependencies.iter().map(|dep| dep.crate_id));
    }
    Roots { found: found.into_boxed_slice(), missing: None }
}

fn is_named(db: &Db, krate: Crate, name: &str) -> bool {
    krate.extra_data(db).display_name.as_ref().is_some_and(|it| it.crate_name().as_str() == name)
}

/// `sched::stack::Yielder::suspend` of the `blaze_graph` `krate`, or the first of its names
/// `krate` lacks.
fn suspend_of(db: &Db, krate: Crate) -> Result<FunctionId, &'static str> {
    let mut module = crate_def_map(db, krate).root_module_id();
    for segment in LAYER {
        match data(db, module).name_in_scope(db, &name_of(segment)).take_types() {
            Some(ModuleDefId::ModuleId(it)) => module = it,
            _ => return Err(segment),
        }
    }
    inherent_method(db, krate, module, ROOT.0, ROOT.1)
}

/// The inherent method `method` of the type `ty` in `module` of `krate`, or the name missing.
fn inherent_method(
    db: &Db,
    krate: Crate,
    module: ModuleId,
    ty: &'static str,
    method: &'static str,
) -> Result<FunctionId, &'static str> {
    let Some(ModuleDefId::AdtId(adt)) =
        data(db, module).name_in_scope(db, &name_of(ty)).take_types()
    else {
        return Err(ty);
    };
    let name = Symbol::intern(method);
    let self_ty = SimplifiedType::Adt(SolverDefId::AdtId(adt));
    InherentImpls::for_crate(db, krate)
        .for_self_ty(&self_ty)
        .iter()
        .find_map(|&imp| {
            imp.impl_items(db).items.iter().find_map(|(it, item)| match *item {
                AssocItemId::FunctionId(func) if *it.symbol() == name => Some(func),
                _ => None,
            })
        })
        .ok_or(method)
}

/// `core::marker::Send` of `krate`'s graph.
#[blaze_query::tracked]
pub(crate) fn send_trait(db: &Db, krate: Crate) -> Option<TraitId> {
    let core = home_crate(db, krate, LangCrateOrigin::Core)?;
    let root = crate_def_map(db, core).root_module_id();
    let Some(ModuleDefId::ModuleId(marker)) =
        data(db, root).name_in_scope(db, &name_of("marker")).take_types()
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
