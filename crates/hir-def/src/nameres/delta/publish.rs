//! What the maintainer publishes (design §9): one [`ScopeInput`] per module, one
//! [`CrateResolution`] per crate, and the singleton [`Resolutions`] that finds the latter. Each is
//! set only when its content changed, so a query reads a change only when its slice changed. A
//! read of a published scope during a maintainer run is noted (`reads`): another crate's, or
//! one of this crate the run does not hold.

use base_db::{Crate, Db};
use hir_expand::name::Name;
use blaze_hash::HashMap;
use blaze_query::db::{Durability, Setter};
use triomphe::Arc;

use super::reads;
use crate::{
    ExternCrateId, FxIndexMap, MacroId, ModuleId, TraitId, UseId,
    item_scope::ItemScope,
    nameres::{
        DefMap, DefMapCrateData, DiagnosticsByModule, LocalDefMap, ModuleData, ModulesMap,
        ModulesSlot, OwnedRecords, RecordsSlot, uses::NameRecords,
    },
    per_ns::PerNs,
};

/// One module's scope as the maintainer last built it: what `ScopeSlot::Input` reads.
#[blaze_query::input]
pub(in crate::nameres) struct ScopeInput {
    #[returns(copy)]
    pub(in crate::nameres) module: ModuleId,
    #[returns(ref)]
    pub(in crate::nameres) scope: ItemScope,
    #[returns(ref)]
    pub(in crate::nameres) children: FxIndexMap<Name, ModuleId>,
}

#[blaze_query::tracked]
impl ScopeInput {
    /// What one name means in the module. Adding or removing another item leaves this equal,
    /// so a query that only looked names up is not run again.
    #[blaze_query::tracked]
    fn name(self, db: &Db, name: Name) -> PerNs {
        self.scope(db).get(&name)
    }

    #[blaze_query::tracked(returns(ref))]
    fn legacy_macro(self, db: &Db, name: Name) -> Option<Box<[MacroId]>> {
        self.scope(db).get_legacy_macro(&name).map(Box::from)
    }

    #[blaze_query::tracked(returns(ref))]
    fn traits(self, db: &Db) -> Box<[TraitId]> {
        // alloc: the memo's value, once per module scope
        self.scope(db).traits().collect()
    }
}

/// A crate's resolution besides its modules' scopes. Set only when the module tree, the crate
/// env, the crate data, the name records or the diagnostics change: an item added to a module
/// leaves it as it is.
#[blaze_query::input]
pub(in crate::nameres) struct CrateResolution {
    #[returns(copy)]
    root: ModuleId,
    #[returns(ref)]
    modules: ModulesMap,
    #[returns(ref)]
    local: LocalDefMap,
    #[returns(ref)]
    data: Arc<DefMapCrateData>,
    #[returns(copy)]
    prelude: Option<(ModuleId, Option<UseId>)>,
    #[returns(ref)]
    macro_use_prelude: HashMap<Name, (MacroId, Option<ExternCrateId>)>,
    #[returns(copy)]
    session_macros: bool,
    #[returns(ref)]
    records: OwnedRecords,
}

/// Every maintained crate's [`CrateResolution`]: changes only when a crate gains or loses its
/// state.
#[blaze_query::input(singleton)]
pub(in crate::nameres) struct Resolutions {
    #[returns(ref)]
    crates: HashMap<Crate, CrateResolution>,
}

fn scope_input(id: blaze_query::db::Id) -> ScopeInput {
    <ScopeInput as blaze_query::db::FromId>::from_id(id)
}

fn resolution(id: blaze_query::db::Id) -> CrateResolution {
    <CrateResolution as blaze_query::db::FromId>::from_id(id)
}

// What `nameres.rs` reads.

pub(in crate::nameres) fn input_scope(db: &Db, id: blaze_query::db::Id) -> &ItemScope {
    let input = scope_input(id);
    if reads::on() {
        reads::note(input.module(db), None);
    }
    input.scope(db)
}

pub(in crate::nameres) fn input_name(db: &Db, id: blaze_query::db::Id, name: Name) -> PerNs {
    let input = scope_input(id);
    if reads::on() {
        reads::note(input.module(db), Some(name));
    }
    *input.name(db, name)
}

pub(in crate::nameres) fn input_legacy_macro(db: &Db, id: blaze_query::db::Id, name: Name) -> Option<&[MacroId]> {
    let input = scope_input(id);
    if reads::on() {
        reads::note(input.module(db), None);
    }
    input.legacy_macro(db, name).as_deref()
}

pub(in crate::nameres) fn input_traits(db: &Db, id: blaze_query::db::Id) -> &[TraitId] {
    let input = scope_input(id);
    if reads::on() {
        reads::note(input.module(db), None);
    }
    input.traits(db)
}

pub(in crate::nameres) fn input_children(db: &Db, id: blaze_query::db::Id) -> &FxIndexMap<Name, ModuleId> {
    let input = scope_input(id);
    if reads::on() {
        reads::note(input.module(db), None);
    }
    input.children(db)
}

pub(in crate::nameres) fn published_modules(db: &Db, id: blaze_query::db::Id) -> &ModulesMap {
    resolution(id).modules(db)
}

pub(in crate::nameres) fn published_module(
    db: &Db,
    id: blaze_query::db::Id,
    module: ModuleId,
) -> Option<&ModuleData> {
    resolution(id).module(db, module).as_ref()
}

#[blaze_query::tracked]
impl CrateResolution {
    /// One module of a published `DefMap`, as `Modules::module` is of a collected one:
    /// depends on the modules there are, and is the same for as long as the module is.
    #[blaze_query::tracked(returns(ref))]
    fn module(self, db: &Db, module: ModuleId) -> Option<ModuleData> {
        self.modules(db).get(&module).cloned()
    }
}

pub(in crate::nameres) fn published_records(db: &Db, id: blaze_query::db::Id) -> &OwnedRecords {
    resolution(id).records(db)
}

/// `krate`'s entry of [`Resolutions`]: what a reader of one crate's resolution depends on, so a
/// crate that gains or loses its resolution leaves every other crate's readers as they were.
#[blaze_query::tracked(returns(copy))]
fn resolution_of(db: &Db, krate: Crate) -> Option<CrateResolution> {
    Resolutions::try_get(db)?.crates(db).get(&krate).copied()
}

/// The crate's def map as the maintainer published it, `None` when no maintainer ran for it.
/// Depends on the crate's resolution minus its modules and records, which its readers read
/// themselves.
pub(in crate::nameres) fn published(db: &Db, krate: Crate) -> Option<(DefMap, LocalDefMap)> {
    let resolution = resolution_of(db, krate)?;
    let id = blaze_query::db::AsId::as_id(&resolution);
    let def_map = DefMap {
        krate,
        block: None,
        root: resolution.root(db),
        modules: ModulesSlot::Published(id),
        prelude: resolution.prelude(db),
        // clone: the crate's `macro_use` prelude, a handful of names, once per def map
        macro_use_prelude: resolution.macro_use_prelude(db).clone(),
        derive_helpers_in_scope: HashMap::default(),
        session_macros: resolution.session_macros(db),
        names: NameRecords::default(),
        // alloc: empty, allocates nothing: the diagnostics are in the records
        diagnostics: Vec::new(),
        diagnostics_by_module: DiagnosticsByModule::default(),
        records: Some(RecordsSlot::Published(id)),
        // clone: one count of the crate's shared data
        data: resolution.data(db).clone(),
    };
    // clone: the extern prelude, one entry per dependency, once per def map
    let local = LocalDefMap { extern_prelude: resolution.local(db).extern_prelude.clone() };
    Some((def_map, local))
}

/// What [`published`] reads of a crate's resolution, by field, to compare across a write: a
/// write that changes none of them runs no `crate_local_def_map` of the crate again.
#[cfg(test)]
pub(in crate::nameres) struct Slices {
    resolutions: HashMap<Crate, CrateResolution>,
    resolution: CrateResolution,
    root: ModuleId,
    local: LocalDefMap,
    data: Arc<DefMapCrateData>,
    prelude: Option<(ModuleId, Option<UseId>)>,
    macro_use_prelude: HashMap<Name, (MacroId, Option<ExternCrateId>)>,
    session_macros: bool,
}

#[cfg(test)]
impl Slices {
    pub(in crate::nameres) fn of(db: &Db, krate: Crate) -> Slices {
        let resolutions = Resolutions::get(db).crates(db);
        let resolution = *resolutions.get(&krate).expect("the crate is maintained");
        Slices {
            resolutions: resolutions.clone(), // clone: a test's snapshot of the crate -> input map
            resolution,
            root: resolution.root(db),
            local: LocalDefMap { extern_prelude: resolution.local(db).extern_prelude.clone() },
            data: resolution.data(db).clone(),
            prelude: resolution.prelude(db),
            macro_use_prelude: resolution.macro_use_prelude(db).clone(),
            session_macros: resolution.session_macros(db),
        }
    }

    /// The fields that differ between `self` and `other`, by name, `data` with both values.
    pub(in crate::nameres) fn differing(&self, other: &Slices) -> Vec<String> {
        let fields = [
            ("resolutions", self.resolutions == other.resolutions),
            ("resolution", self.resolution == other.resolution),
            ("root", self.root == other.root),
            ("local", self.local == other.local),
            ("data", self.data == other.data),
            ("prelude", self.prelude == other.prelude),
            ("macro_use_prelude", self.macro_use_prelude == other.macro_use_prelude),
            ("session_macros", self.session_macros == other.session_macros),
        ];
        let describe = |name: &str| match name {
            "data" => format!("data: {:?} then {:?}", self.data, other.data),
            _ => name.to_owned(),
        };
        fields.into_iter().filter(|&(_, same)| !same).map(|(name, _)| describe(name)).collect()
    }
}

// What the maintainer sets.

/// A module's scope input after a run: made for a new module, set where the scope or the
/// children changed. Whether it set anything.
pub(super) fn set_scope(
    db: &mut Db,
    slot: &mut Option<ScopeInput>,
    module: ModuleId,
    scope: ItemScope,
    children: FxIndexMap<Name, ModuleId>,
) -> bool {
    let Some(input) = *slot else {
        *slot = Some(ScopeInput::new(db, module, scope, children));
        return true;
    };
    let mut set = false;
    if *input.scope(db) != scope {
        input.set_scope(db).to(scope);
        set = true;
    }
    if *input.children(db) != children {
        input.set_children(db).to(children);
        set = true;
    }
    set
}

pub(super) fn scope_id(input: ScopeInput) -> blaze_query::db::Id {
    blaze_query::db::AsId::as_id(&input)
}

/// Sets a module's scope alone, its children as they are, where it changed: whether it did.
pub(super) fn set_scope_alone(db: &mut Db, input: ScopeInput, scope: ItemScope) -> bool {
    if *input.scope(db) == scope {
        return false;
    }
    input.set_scope(db).to(scope);
    true
}

/// What a crate's [`CrateResolution`] holds, as the maintainer assembled it. `modules` and
/// `records` are `None` where the write left them as published: they are the crate-sized
/// fields, neither compared nor copied then.
pub(super) struct Assembled {
    pub(super) root: ModuleId,
    pub(super) modules: Option<ModulesMap>,
    pub(super) local: LocalDefMap,
    pub(super) data: Arc<DefMapCrateData>,
    pub(super) prelude: Option<(ModuleId, Option<UseId>)>,
    pub(super) macro_use_prelude: HashMap<Name, (MacroId, Option<ExternCrateId>)>,
    pub(super) session_macros: bool,
    pub(super) records: Option<OwnedRecords>,
}

/// Sets each field of the crate's resolution that differs from `new` (makes the input for a
/// crate first seen, and enters it in [`Resolutions`]); the number of fields set.
pub(super) fn set_resolution(
    db: &mut Db,
    krate: Crate,
    slot: &mut Option<CrateResolution>,
    new: Assembled,
) -> u64 {
    let Assembled { root, modules, local, data, prelude, macro_use_prelude, session_macros, records } = new;
    let Some(it) = *slot else {
        let it = CrateResolution::new(
            db,
            root,
            modules.expect("nameres delta: a crate's first resolution has its modules"),
            local,
            data,
            prelude,
            macro_use_prelude,
            session_macros,
            records.expect("nameres delta: a crate's first resolution has its records"),
        );
        *slot = Some(it);
        enter(db, krate, Some(it));
        return 1;
    };
    let mut set = 0;
    if it.root(db) != root {
        it.set_root(db).to(root);
        set += 1;
    }
    if let Some(modules) = modules
        && *it.modules(db) != modules
    {
        it.set_modules(db).to(modules);
        set += 1;
    }
    if *it.local(db) != local {
        it.set_local(db).to(local);
        set += 1;
    }
    if *it.data(db) != data {
        it.set_data(db).to(data);
        set += 1;
    }
    if it.prelude(db) != prelude {
        it.set_prelude(db).to(prelude);
        set += 1;
    }
    if *it.macro_use_prelude(db) != macro_use_prelude {
        it.set_macro_use_prelude(db).to(macro_use_prelude);
        set += 1;
    }
    if it.session_macros(db) != session_macros {
        it.set_session_macros(db).to(session_macros);
        set += 1;
    }
    if let Some(records) = records
        && *it.records(db) != records
    {
        it.set_records(db).to(records);
        set += 1;
    }
    set
}

/// Enters `krate`'s resolution in [`Resolutions`] (`None` takes it out).
pub(super) fn enter(db: &mut Db, krate: Crate, resolution: Option<CrateResolution>) {
    match Resolutions::try_get(db) {
        Some(all) => {
            if all.crates(db).get(&krate).copied() == resolution {
                return;
            }
            // clone: the crate -> input map, once per crate gained or lost
            let mut crates = all.crates(db).clone();
            match resolution {
                Some(it) => crates.insert(krate, it),
                None => crates.remove(&krate),
            };
            all.set_crates(db).to(crates);
        }
        None => {
            let mut crates = HashMap::default();
            if let Some(it) = resolution {
                crates.insert(krate, it);
            }
            _ = Resolutions::builder(crates).durability(Durability::LOW).new(db);
        }
    }
}
