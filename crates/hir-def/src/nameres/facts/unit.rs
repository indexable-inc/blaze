//! A unit: the resolution state of a set of modules resolved together, the import side of the
//! old `DefCollector` (`nameres-core.md` §5).
//!
//! A crate's unit is one strongly connected component of a round's import graph
//! ([`scc_state`]): its view is a `DefMap` in `ModulesSlot::Unit` holding the members' rows,
//! every other module of the crate read through its round's row, whose scope is a fact
//! (`ScopeSlot::Fact`). The unit replays its members' ops (`nameres-core.md` §3), runs the old
//! worklist to its fixed point, and hands each member's scope to the database as a
//! `ModuleScope`. Every read of a module it does not hold is checked against the partition
//! before it is made: a read the partition does not put upstream would be a query cycle, and
//! stops naming the import. A block's unit ([`Unit::block`]) holds every module of the block's
//! map and is the sink the block's collection writes through.

mod imports;
#[cfg(test)]
mod tests;

use std::{
    alloc::{Allocator, Global},
    mem,
};

use base_db::Db;
use blaze_hash::{FixedState, HashMap};
use blaze_mem::Frame;
use hashbrown::HashSet;
use hir_expand::{
    mod_path::{ModPath, PathKind},
    name::Name,
};
use span::Edition;
use triomphe::Arc;

use crate::{
    AdtId, FxIndexMap, Lookup, MacroId, ModuleDefId, ModuleId, UseId,
    item_scope::{
        GlobAmbiguity, GlobId, ImportId, ImportOrExternCrate, ImportOrGlob, ItemInNs, ItemScope,
        PerNsGlobImports, UnderscoreImport,
    },
    item_tree::{ImportAlias, ImportKind},
    nameres::{
        BuildingScope, BuiltinShadowMode, DefMap, DiagnosticsByModule, LocalDefMap, ModuleData,
        ModuleOrigin, ModuleScope, ModulesMap, ModulesSlot, ResolveMode, ScopeSlot,
        diagnostics::DefDiagnostic,
        path_resolution::{PathRead, ReachedFixedPoint, ResolvePathResult},
        trait_items::trait_item_per_ns,
        uses,
    },
    per_ns::{Item, PerNs},
    visibility::{RawVisibility, Visibility, VisibilityError, visibility_path},
};

use self::imports::{
    ALL_NS, GlobEdges, ImportList, ImportQueues, ImporterEdge, ImporterLists, Imports, MACROS,
    NONE, PartialResolvedImport, Phase, TYPES, WaitOn, written_ns,
};
use super::{
    FrameSet, FrameVec,
    env::{ChildMode, ChildRequest, CollectCx, Sink},
    graph::Partition,
    ops::{DeclOp, ImportOp, MacroOp, OpLog},
    queries::{
        self, CrateEnvKey, FactKey, RoundActive, RoundKey, RoundKind, Rows, RowsKey, SccKey,
        SccState,
    },
    set_in,
};

const GLOB_RECURSION_LIMIT: usize = 100;

/// What a component carries from one round to the next (`A = Global`): its members' rows, its
/// imports and what each resolved to, its glob edges, and what it read of other components. The
/// worklist over the imports is each run's own ([`ImportQueues`]). A block's state, never
/// carried, is in its query's frame; its rows go into the block's map.
#[derive(Clone, Debug)]
pub(crate) struct UnitState<A: Allocator + Clone = Global> {
    /// The members' rows, in the order they were declared. Moved into the view while a unit
    /// runs, and back when it is done.
    rows: FxIndexMap<ModuleId, ModuleData>,
    imports: Imports<A>,
    globs: GlobEdges<A>,
    from_glob_import: PerNsGlobImports<A>,
    /// Every module of another component the state read (a path's slot, a glob's source).
    upstream: HashSet<ModuleId, FixedState, A>,
    /// The modules of other components the members glob-import, in the order first read:
    /// pulled again when the component carries on into a round.
    upstream_globs: Vec<ModuleId, A>,
    /// The `#[prelude_import]` a member resolved.
    prelude: Option<(ModuleId, Option<UseId>)>,
}

impl Default for UnitState {
    fn default() -> Self {
        UnitState::new_in(Global)
    }
}

impl<A: Allocator + Clone> UnitState<A> {
    pub(crate) fn new_in(alloc: A) -> Self {
        // clone: the allocator, a `&Frame` or `Global`: no allocation.
        let a = || alloc.clone();
        UnitState {
            rows: FxIndexMap::default(),
            imports: Imports::new_in(a()),
            globs: ImporterLists::new_in(a()),
            from_glob_import: PerNsGlobImports::new_in(a()),
            upstream: HashSet::with_hasher_in(FixedState, a()),
            upstream_globs: Vec::new_in(a()),
            prelude: None,
        }
    }

    /// Every import of the members and what it resolved to, `None` for one still unresolved,
    /// in import order.
    pub(crate) fn import_outcomes(&self) -> impl Iterator<Item = (&ImportOp, Option<PerNs>)> + '_ {
        let imports = &self.imports;
        imports.directives.iter().zip(&imports.list).map(|(it, list)| {
            let resolved = (*list != ImportList::Unresolved).then(|| it.status.namespaces());
            (&it.op, resolved)
        })
    }
}

/// What a crate unit checks each read of a module it does not hold against.
#[derive(Clone, Copy)]
pub(super) struct ReadCheck<'db> {
    pub(super) partition: &'db Partition,
    /// The round's module order (`RoundModules::index`).
    pub(super) index: &'db HashMap<ModuleId, u32>,
    pub(super) component: u32,
}

impl ReadCheck<'_> {
    fn at(&self, module: ModuleId) -> u32 {
        match self.index.get(&module) {
            Some(&at) => at,
            None => panic!("nameres facts: {module:?} is read but is not a module of the round"),
        }
    }

    fn is_member(&self, module: ModuleId) -> bool {
        self.partition.of(self.at(module)) == self.component
    }

    fn may_read(&self, module: ModuleId) -> bool {
        self.partition.may_read(self.component, self.partition.of(self.at(module)))
    }
}

/// Whether `module`, a module of the unit's own map, is one the unit holds. A read of any other
/// is checked against the partition and recorded as upstream; a read the partition does not put
/// upstream stops, naming `path`, the import of `importer` that made it.
fn held<A: Allocator + Clone>(
    db: &Db,
    check: &Option<ReadCheck<'_>>,
    upstream: &mut HashSet<ModuleId, FixedState, A>,
    module: ModuleId,
    path: &ModPath,
    importer: ModuleId,
) -> bool {
    let Some(check) = check else { return true };
    if check.is_member(module) {
        return true;
    }
    if !check.may_read(module) {
        panic!(
            "nameres facts: import `{}` of {importer:?} reads {module:?}, which the partition does not put upstream",
            path.display(db, Edition::LATEST),
        );
    }
    upstream.insert(module);
    false
}

/// The extern prelude a unit's paths read: the crate's (a block's), or the round's.
enum Local<'db> {
    Borrowed(&'db LocalDefMap),
    Owned(LocalDefMap),
}

impl Local<'_> {
    fn get(&self) -> &LocalDefMap {
        match self {
            Local::Borrowed(it) => it,
            Local::Owned(it) => it,
        }
    }
}

/// A unit at work: the view `DefMap` paths resolve in, the extern prelude, the state, and, for a
/// crate's component, the partition its reads are checked against. What the run keeps besides
/// the state is in the frame of the query that runs it.
pub(crate) struct Unit<'db, 'f, A: Allocator + Clone = Global> {
    db: &'db Db,
    frame: &'f Frame,
    def_map: DefMap,
    local: Local<'db>,
    state: UnitState<A>,
    queues: ImportQueues<'f>,
    check: Option<ReadCheck<'db>>,
    /// The macro directives a block's collection made (`Sink::apply(DeclOp::Macro)`), until
    /// `take_macros` moves them.
    macros: FrameVec<'f, MacroOp>,
    /// Scratch for `update`: the resolutions of each level of a glob propagation, one level's
    /// after its caller's.
    held: FrameVec<'f, (Option<Name>, PerNs)>,
    /// The members whose row an op or a resolution wrote since the unit started.
    touched: FrameSet<'f, ModuleId>,
    /// The names of `extern crate`s that did not resolve (`DeclOp::UnresolvedExternCrate`).
    unresolved_extern_crates: FrameSet<'f, Name>,
    /// The `NotIntroducing`, `ResolvedMacroPath` and `DeriveHelperUse` ops a block's collection
    /// made, in order, for its `NameRecords`.
    records: FrameVec<'f, DeclOp>,
    /// The diagnostics a block's collection made, in order, until the block's map takes them.
    diagnostics: FrameVec<'f, DefDiagnostic>,
    /// Whether a macro directive of the crate (a block's, a region's) is still unexpanded, as
    /// the driver last said ([`Unit::set_unexpanded`]).
    unexpanded: bool,
    /// What glob propagation (`update_recursive`) did since the unit started: the names it
    /// pushed into a scope and the importer edges it walked, for the `nameres update` span.
    names_pushed: u64,
    edges_walked: u64,
}

/// The rows of a unit's view.
fn own_rows(modules: &mut ModulesSlot) -> &mut ModulesMap {
    match modules {
        ModulesSlot::Building(own)
        | ModulesSlot::Unit { own, .. }
        | ModulesSlot::Maintained { own, .. } => own,
        ModulesSlot::Collected(_) | ModulesSlot::Published(_) => {
            panic!("nameres facts: a unit's view is a collected `DefMap`")
        }
    }
}

/// The row of `module`, a member, to write.
fn row_of<'a>(
    modules: &'a mut ModulesSlot,
    touched: &mut FrameSet<'_, ModuleId>,
    module: ModuleId,
) -> &'a mut ModuleData {
    touched.insert(module);
    match own_rows(modules).get_mut(&module) {
        Some(row) => row,
        None => panic!("nameres facts: an op writes {module:?}, which the unit does not hold"),
    }
}

/// Where glob `glob` is in its module's source, the order that ties two globs giving one name
/// different items (docs/v2/plan/design/dbsp.md §6): its `use` item's range in the file the
/// module's source is in (a macro's expansion at its call), then its range in its own file,
/// then its use tree. Asked only on such a tie.
fn glob_position(db: &Db, glob: GlobId) -> (span::TextSize, span::TextSize, u32) {
    let id = glob.use_.lookup(db).id;
    let range = id.to_range(db);
    let original = hir_expand::InFile::new(id.file_id, range).original_node_file_range_rooted(db);
    (original.range.start(), range.start(), u32::from(glob.idx.into_raw()))
}

/// Whether `glob`, giving a name at `vis` the item another glob (`prev`, at `prev_vis`)
/// already gave it, is the one the name is bound through: the wider, and of two as wide the
/// first in the source ([`glob_position`]), so the binding is the same whichever arrives first.
/// Both collectors ask it.
pub(in crate::nameres) fn glob_takes_over(
    db: &Db,
    def_map: &DefMap,
    glob: GlobId,
    vis: Visibility,
    prev: Option<GlobId>,
    prev_vis: Visibility,
) -> bool {
    if vis != prev_vis {
        return vis.max(db, prev_vis, def_map) == Some(vis);
    }
    prev.is_some_and(|prev| prev != glob && glob_position(db, glob) < glob_position(db, prev))
}

/// The visibility of `scope`'s record of `glob` giving `name` the item `other`, if it has one.
pub(in crate::nameres) fn recorded_ambiguity(
    scope: &ItemScope,
    name: &Name,
    other: ItemInNs,
    glob: GlobId,
) -> Option<Visibility> {
    let same = |it: &&GlobAmbiguity| it.name == *name && it.glob == glob && it.other == other;
    scope.glob_ambiguities().iter().find(same).map(|it| it.vis)
}

/// The visibility to record a glob giving a name an item at `vis` where an earlier record of
/// the same glob and item has `earlier` ([`recorded_ambiguity`]): the wider, as rustc keeps the
/// more visible of a glob's two bindings of one item, so the record is the same whichever of
/// the glob's deliveries arrives last. Both collectors ask it.
pub(in crate::nameres) fn widest_ambiguity(
    db: &Db,
    def_map: &DefMap,
    earlier: Option<Visibility>,
    vis: Visibility,
) -> Visibility {
    earlier.and_then(|it| it.max(db, vis, def_map)).unwrap_or(vis)
}

/// Whether the type or value binding of `def`, an import's resolution, is a glob's of a
/// module of `krate`, which an expansion there may still shadow. A macro binding is
/// determined whatever brought it, as in rustc.
fn through_glob_of(db: &Db, krate: base_db::Crate, def: &PerNs) -> bool {
    let here = |glob: GlobId| glob.use_.lookup(db).container.krate(db) == krate;
    matches!(def.types.and_then(|it| it.import), Some(ImportOrExternCrate::Glob(glob)) if here(glob))
        || matches!(def.values.and_then(|it| it.import), Some(ImportOrGlob::Glob(glob)) if here(glob))
}

impl<'db, 'f> Unit<'db, 'f, &'f Frame> {
    /// A block's unit: `def_map` is `block_def_map`'s empty map (`ModulesSlot::Building`),
    /// every module it holds is a member, no partition check. Its state is in the frame.
    pub(crate) fn block(
        cx: &CollectCx<'db, 'f>,
        def_map: DefMap,
        local: &'db LocalDefMap,
    ) -> Unit<'db, 'f, &'f Frame> {
        let state = UnitState::new_in(cx.frame);
        Unit::new(cx.db, cx.frame, def_map, Local::Borrowed(local), state, None)
    }

    /// The diagnostics the block's collection made, in order.
    pub(crate) fn take_diagnostics(&mut self) -> FrameVec<'f, DefDiagnostic> {
        mem::replace(&mut self.diagnostics, Vec::new_in(self.frame))
    }

    /// A run of the name resolution maintainer (`nameres::delta`): `def_map` is its
    /// `ModulesSlot::Maintained` view, every module it holds a member, no partition check; paths
    /// read the crate env `env`. Its state is in the frame.
    pub(crate) fn region(
        cx: &CollectCx<'db, 'f>,
        def_map: DefMap,
        env: &super::env::CrateEnv,
    ) -> Unit<'db, 'f, &'f Frame> {
        let mut def_map = def_map;
        env.install(&mut def_map);
        let state = UnitState::new_in(cx.frame);
        Unit::new(cx.db, cx.frame, def_map, Local::Owned(env.local_def_map()), state, None)
    }

    /// The crate env changed under a run (the crate root's expansion made an `extern crate`,
    /// a `#[macro_use]` or the std prelude): paths read the new one, and every import is
    /// tried again.
    pub(crate) fn set_env(&mut self, env: &super::env::CrateEnv) {
        let prelude = self.def_map.prelude;
        env.install(&mut self.def_map);
        if self.state.prelude.is_some() {
            self.def_map.prelude = prelude;
        }
        self.local = Local::Owned(env.local_def_map());
        self.queues.wake_all(&self.state.imports);
    }
}

impl<'db, 'f, A: Allocator + Clone> Unit<'db, 'f, A> {
    fn new(
        db: &'db Db,
        frame: &'f Frame,
        def_map: DefMap,
        local: Local<'db>,
        state: UnitState<A>,
        check: Option<ReadCheck<'db>>,
    ) -> Unit<'db, 'f, A> {
        let queues = ImportQueues::new_in(frame, state.imports.len());
        Unit {
            db,
            frame,
            def_map,
            local,
            state,
            queues,
            check,
            macros: Vec::new_in(frame),
            held: Vec::new_in(frame),
            touched: set_in(frame),
            unresolved_extern_crates: set_in(frame),
            records: Vec::new_in(frame),
            diagnostics: Vec::new_in(frame),
            unexpanded: false,
            names_pushed: 0,
            edges_walked: 0,
        }
    }
}

impl<'db, 'f> Unit<'db, 'f> {
    /// A crate's component at `round`: the members' rows of `state` (none for a fresh one), the
    /// round's view of every other module, the round's crate env, and `prelude`, the std
    /// prelude another component's `#[prelude_import]` resolved.
    pub(super) fn component(
        db: &'db Db,
        frame: &'f Frame,
        round: RoundKey,
        env: CrateEnvKey,
        check: Option<ReadCheck<'db>>,
        mut state: UnitState,
        prelude: Option<(ModuleId, Option<UseId>)>,
    ) -> Unit<'db, 'f> {
        let krate = round.krate(db);
        let rows = RowsKey::new(db, Rows::Round(round));
        let own = ModulesMap { inner: mem::take(&mut state.rows) };
        let mut def_map = DefMap {
            krate,
            block: None,
            root: queries::crate_root(db, krate),
            modules: ModulesSlot::Unit { own, rows: blaze_query::db::AsId::as_id(&rows) },
            prelude: None,
            macro_use_prelude: HashMap::default(),
            derive_helpers_in_scope: HashMap::default(),
            session_macros: false,
            names: uses::NameRecords::default(),
            // alloc: empty, a component's view keeps no diagnostics
            diagnostics: Vec::new(),
            diagnostics_by_module: DiagnosticsByModule::default(),
            records: None,
            // clone: one count of the crate's shared data.
            data: queries::crate_data(db, krate).clone(),
        };
        let env = env.env(db);
        env.install(&mut def_map);
        if let Some(prelude) = state.prelude.or(prelude) {
            def_map.prelude = Some(prelude);
        }
        Unit::new(db, frame, def_map, Local::Owned(env.local_def_map()), state, check)
    }
}

impl<'db, 'f, A: Allocator + Clone> Unit<'db, 'f, A> {
    /// Whether a macro directive that may still declare names is unexpanded, said by the driver
    /// before each import fixed point. While one is, an import whose type or value binding came
    /// through a glob of this crate is indeterminate, not resolved: rustc's glob determinacy
    /// (`rustc_resolve` `DeclKind::determined`, `resolve_ident_in_module_globs_unadjusted`), so
    /// it is retried and binds the name an expansion shadows the glob with. rustc asks only the
    /// glob importers on the binding's chain; asking about any directive retries more often and
    /// binds the same.
    pub(crate) fn set_unexpanded(&mut self, unexpanded: bool) {
        self.unexpanded = unexpanded;
    }

    /// The import fixed point (`resolve_imports` until `ReachedFixedPoint::Yes`).
    pub(crate) fn resolve_imports(&mut self) {
        while self.resolve_imports_pass() == ReachedFixedPoint::No {}
    }

    /// `collect`'s end: `record_resolved_import` of every still-unresolved import.
    pub(crate) fn record_unresolved(&mut self) {
        for at in 0..self.state.imports.len() {
            if self.state.imports.list[at as usize] == ImportList::Unresolved {
                self.record_resolved_import(at);
            }
        }
    }

    /// The still-unresolved imports, in import order.
    pub(crate) fn unresolved_imports(&self) -> impl Iterator<Item = &ImportOp> + '_ {
        self.state.imports.unresolved().map(|it| &it.op)
    }

    /// The names of the `extern crate`s that did not resolve: an unresolved import whose path
    /// starts with one gets no diagnostic (`finish`).
    pub(crate) fn unresolved_extern_crates(&self) -> &FrameSet<'f, Name> {
        &self.unresolved_extern_crates
    }

    /// Every import and what it resolved to, `None` for one still unresolved, in import order:
    /// what `NameRecords` resolves again.
    pub(crate) fn import_outcomes(&self) -> impl Iterator<Item = (&ImportOp, Option<PerNs>)> + '_ {
        self.state.import_outcomes()
    }

    /// The `NotIntroducing`, `ResolvedMacroPath` and `DeriveHelperUse` ops the collection
    /// made, in order.
    pub(crate) fn records(&self) -> &[DeclOp] {
        &self.records
    }

    pub(crate) fn def_map_mut(&mut self) -> &mut DefMap {
        &mut self.def_map
    }

    pub(crate) fn into_def_map(self) -> DefMap {
        self.def_map
    }

    /// The state, the members' rows back in it.
    fn into_state(mut self) -> UnitState<A> {
        self.state.rows = mem::take(&mut own_rows(&mut self.def_map.modules).inner);
        self.state
    }

    /// The members' ops of `round`, as the contract's §3 says a unit applies them: from a
    /// round-0 log those that target the module (and, in the root's, the `Splice`s); from a
    /// later round's every resolution op. A later round at which it gains nothing
    /// (`RoundModules::GAINS`) has none, and its `module_round` is not asked.
    pub(super) fn replay_round(&mut self, module: ModuleId, round: u32) {
        let db = self.db;
        let is_root = module == self.def_map.root;
        let rows = queries::round_modules(db, RoundKey::new(db, self.def_map.krate, round));
        let ops: &'db OpLog = match round {
            0 => {
                let key = rows.row(module).and_then(|row| row.decl).unwrap_or_else(|| {
                    panic!("nameres facts: {module:?} is a module of round 0 without a round-0 key")
                });
                &queries::module_decls(db, key).ops
            }
            round => {
                let at = rows.at(module).unwrap_or_else(|| {
                    panic!("nameres facts: {module:?} is not a module of round {round}")
                });
                if !rows.gains(at) {
                    return;
                }
                &queries::module_round(db, FactKey::new(db, module, self.def_map.krate, round)).ops
            }
        };
        for op in ops.iter() {
            let applies = match op {
                DeclOp::Splice { .. } => is_root && round == 0,
                op => op.target() == Some(module) && op.is_resolution_op(),
            };
            if applies {
                self.replay(op);
            }
        }
    }

    /// Applies an op another query owns.
    pub(crate) fn replay(&mut self, op: &DeclOp) {
        let db = self.db;
        match op {
            DeclOp::Init { module, origin, visibility, parent, inherited } => {
                self.insert_row(*module, *origin, *visibility, *parent, inherited)
            }
            DeclOp::Splice { key, .. } => {
                for op in queries::subtree_macro_exports(db, *key).iter() {
                    self.replay(op);
                }
            }
            &DeclOp::PushChild { parent, name, child } => {
                let replaced = row_of(&mut self.def_map.modules, &mut self.touched, parent)
                    .children_mut()
                    .insert(name, child);
                if replaced.is_some() {
                    // A path can read whether a module is its parent's `__blaze_test_harness`.
                    self.queues.wake_all(&self.state.imports);
                }
            }
            // The collection keeps it in its `SideTables`.
            DeclOp::SetModDir { .. } => {}
            &DeclOp::Declare { module, def } => {
                self.scope_mut(module).declare(def);
            }
            DeclOp::Update { module, resolutions, vis, import } => {
                self.update(*module, resolutions, *vis, *import);
            }
            &DeclOp::DefineImpl { module, imp, is_trait_impl, is_reservation } => {
                self.scope_mut(module).define_impl(imp, is_trait_impl, is_reservation);
            }
            &DeclOp::DefineBuiltinDeriveImpl { module, imp } => {
                self.scope_mut(module).define_builtin_derive_impl(imp);
            }
            &DeclOp::DefineExternBlock { module, id } => {
                self.scope_mut(module).define_extern_block(id);
            }
            &DeclOp::DefineExternCrateDecl { module, id } => {
                self.scope_mut(module).define_extern_crate_decl(id);
            }
            &DeclOp::DefineUseDecl { module, id } => {
                self.scope_mut(module).define_use_decl(id);
            }
            &DeclOp::DefineUnnamedConst { module, id } => {
                self.scope_mut(module).define_unnamed_const(id);
            }
            &DeclOp::DefineLegacyMacro { module, name, mac } => {
                self.scope_mut(module).define_legacy_macro(name, mac);
                self.queues.wake_env(&self.state.imports, name, MACROS);
            }
            &DeclOp::AddMacroInvoc { module, call, id } => {
                self.scope_mut(module).add_macro_invoc(call, id);
            }
            &DeclOp::AddAttrMacroInvoc { module, item, id } => {
                self.scope_mut(module).add_attr_macro_invoc(item, id);
            }
            &DeclOp::SetDeriveMacroInvoc { module, adt, call, attr, idx } => {
                self.scope_mut(module).set_derive_macro_invoc(adt, call, attr, idx);
            }
            DeclOp::InitDeriveAttribute { module, adt, attr, attr_call, derive_calls } => {
                // clone: a `SmallVec` of four, inline for all but the longest `#[derive]`s.
                let derive_calls = derive_calls.clone();
                self.scope_mut(*module).init_derive_attribute(*adt, *attr, *attr_call, derive_calls);
            }
            // clone: the worklist owns its imports across rounds; once per import and component.
            DeclOp::Import(it) => self.state.imports.push(&mut self.queues, it.clone()),
            // clone: a block's directive, once.
            DeclOp::Macro(it) => self.macros.push(it.clone()),
            &DeclOp::Prelude { module, import } => {
                self.def_map.prelude = Some((module, import));
                self.queues.wake_all(&self.state.imports);
            }
            &DeclOp::UnresolvedExternCrate { name } => {
                self.unresolved_extern_crates.insert(name);
            }
            DeclOp::SessionMacros => self.def_map.session_macros = true,
            DeclOp::Diagnostic(diagnostic) => panic!(
                "nameres facts: a unit is given the diagnostic {diagnostic:?} by reference; diagnostics go through `Sink::apply`"
            ),
            DeclOp::NotIntroducing(_) | DeclOp::ResolvedMacroPath { .. } | DeclOp::DeriveHelperUse(_) => {
                panic!(
                    "nameres facts: a unit is given the name record {op:?} by reference; records go through `Sink::apply`"
                )
            }
            DeclOp::ExternPrelude { .. }
            | DeclOp::MacroUsePrelude { .. }
            | DeclOp::ExportedDerive { .. }
            | DeclOp::ProcMacroFn { .. } => panic!(
                "nameres facts: the crate-level op {op:?} reached a unit; only the crate root's collection makes it, into the round's env"
            ),
        }
    }

    fn scope_mut(&mut self, module: ModuleId) -> &mut crate::item_scope::ItemScope {
        row_of(&mut self.def_map.modules, &mut self.touched, module).scope_mut()
    }

    /// The row of a module that comes to exist (`push_child_module`'s `modules.insert`), with
    /// the parent's textual scope where it is declared: its `Init`. A crate unit replaying a
    /// round already holding the row (a carried member) keeps it.
    fn insert_row(
        &mut self,
        module: ModuleId,
        origin: ModuleOrigin,
        visibility: Visibility,
        parent: Option<ModuleId>,
        inherited: &[(Name, Box<[MacroId]>)],
    ) {
        let rows = own_rows(&mut self.def_map.modules);
        if rows.contains_key(&module) {
            return;
        }
        let mut data = ModuleData::new(origin, visibility, parent);
        for (name, macs) in inherited {
            for &mac in macs {
                data.scope_mut().define_legacy_macro(*name, mac);
            }
        }
        rows.insert(module, data);
        self.touched.insert(module);
    }

    /// One pass over the imports: every unresolved one in order, then every indeterminate one
    /// in the order they became so, as the loop over all of them was. Only those a write since
    /// their last attempt woke are retried: each other one would resolve as it did, recording
    /// nothing. So the pass records what that loop records, in its order.
    fn resolve_imports_pass(&mut self) -> ReachedFixedPoint {
        let mut res = ReachedFixedPoint::Yes;
        self.queues.begin_pass(&self.state.imports);
        while let Some(at) = self.queues.next_unresolved(&self.state.imports) {
            if self.retry_unresolved(at) {
                res = ReachedFixedPoint::No;
            }
        }
        // Macro expansion can expose a new binding or change the visibility of an existing
        // one without adding a namespace. Revisit partial imports until their bindings settle.
        self.queues.phase = Phase::Indeterminate(0);
        while let Some(seq) = self.queues.next_indeterminate(&self.state.imports) {
            if self.retry_indeterminate(seq) {
                res = ReachedFixedPoint::No;
            }
        }
        self.queues.phase = Phase::Idle;
        res
    }

    /// Retries unresolved import `at`; whether it recorded anything.
    fn retry_unresolved(&mut self, at: u32) -> bool {
        let i = at as usize;
        let _reading = crate::nameres::delta::reads::reading(self.state.imports.directives[i].op.module);
        self.queues.attempt(at);
        let status = self.resolve_import(at);
        let imports = &mut self.state.imports;
        imports.directives[i].status = status;
        match status {
            PartialResolvedImport::Unresolved => {
                self.queues.wait(at);
                false
            }
            PartialResolvedImport::Indeterminate(resolved) => {
                imports.seq[i] = imports.indeterminate.len() as u32;
                imports.indeterminate.push((at, resolved));
                imports.list[i] = ImportList::Indeterminate;
                // Before recording: what it records may change what it resolves to.
                self.queues.wait(at);
                self.record_resolved_import(at);
                true
            }
            PartialResolvedImport::Resolved(_) => {
                imports.list[i] = ImportList::Resolved;
                self.record_resolved_import(at);
                true
            }
        }
    }

    /// Retries the indeterminate import at `seq`; whether it recorded anything.
    fn retry_indeterminate(&mut self, seq: u32) -> bool {
        let (at, settled) = self.state.imports.indeterminate[seq as usize];
        let i = at as usize;
        let _reading = crate::nameres::delta::reads::reading(self.state.imports.directives[i].op.module);
        self.queues.attempt(at);
        let status = self.resolve_import(at);
        let imports = &mut self.state.imports;
        imports.directives[i].status = status;
        match status {
            PartialResolvedImport::Indeterminate(import) if import != settled => {
                imports.indeterminate[seq as usize].1 = import;
                self.queues.wait(at);
                self.record_resolved_import(at);
                true
            }
            PartialResolvedImport::Resolved(_) => {
                imports.list[i] = ImportList::Resolved;
                self.record_resolved_import(at);
                true
            }
            _ => {
                self.queues.wait(at);
                false
            }
        }
    }

    /// Resolves import `at`, leaving in `queues.keys` (and `queues.keys_any`) what the lookup
    /// read that this unit can still change, for `wait`. A read of a module of this crate the
    /// unit does not hold is checked against the partition and recorded as upstream; it waits on
    /// nothing, as that module's scope is final for the round.
    fn resolve_import(&mut self, at: u32) -> PartialResolvedImport {
        let db = self.db;
        let unexpanded = self.unexpanded;
        let Unit { def_map, local, state, queues, check, .. } = self;
        let UnitState { imports, upstream, .. } = state;
        let (keys, keys_any) = (&mut queues.keys, &mut queues.keys_any);
        keys.clear();
        *keys_any = false;
        let directive = &imports.directives[at as usize];
        let (path, importer) = (&directive.op.path, directive.op.module);
        let last = path.segments().last().copied();
        let in_crate = def_map.block.is_none();
        let (krate, block) = (def_map.krate, def_map.block_id());
        let local_to = |m: ModuleId| m.krate(db) == krate && m.block(db) == block;
        // The def maps of other crates and of the blocks around this one are collected, so what
        // the lookup reads there waits on nothing. A name other than the last is read for the
        // module or type it names.
        let mut reads = |read: PathRead| match read {
            PathRead::Slot(module, name) if local_to(module) => {
                if held(db, check, upstream, module, path, importer) {
                    let ns = if Some(name) == last { ALL_NS } else { TYPES };
                    keys.push((WaitOn::Slot(module, name), ns));
                }
            }
            PathRead::LegacyMacro(module, name) if local_to(module) => {
                keys.push((WaitOn::Env(name), MACROS));
            }
            PathRead::ExternPrelude(name) if in_crate => keys.push((WaitOn::Env(name), TYPES)),
            PathRead::MacroUsePrelude(name) => keys.push((WaitOn::Env(name), MACROS)),
            // A new std prelude module wakes every import.
            PathRead::Prelude => {}
            PathRead::Unscoped => *keys_any = true,
            PathRead::Slot(..) | PathRead::LegacyMacro(..) | PathRead::ExternPrelude(_) => {}
        };
        let ResolvePathResult { resolved_def, segment_index, reached_fixedpoint, prefix_info } =
            def_map.resolve_path_fp_recording(
                local.get(),
                db,
                ResolveMode::Import,
                importer,
                path,
                BuiltinShadowMode::Module,
                None, // An import may resolve to any kind of macro.
                &mut reads,
            );

        if reached_fixedpoint == ReachedFixedPoint::No
            || resolved_def.is_none()
            || segment_index.is_some()
        {
            return PartialResolvedImport::Unresolved;
        }

        if prefix_info.differing_crate {
            return PartialResolvedImport::Resolved(
                resolved_def.filter_visibility(|v| matches!(v, Visibility::Public)),
            );
        }

        // Check whether all namespaces are resolved.
        if resolved_def.is_full() {
            match unexpanded && through_glob_of(db, krate, &resolved_def) {
                true => PartialResolvedImport::Indeterminate(resolved_def),
                false => PartialResolvedImport::Resolved(resolved_def),
            }
        } else {
            PartialResolvedImport::Indeterminate(resolved_def)
        }
    }

    /// `DefMap::resolve_visibility` of an import's visibility, with the reads of modules the
    /// unit does not hold checked and recorded as the import's own are.
    fn resolve_visibility(
        &mut self,
        module: ModuleId,
        visibility: &RawVisibility,
        import: &ModPath,
    ) -> Result<Visibility, VisibilityError> {
        let db = self.db;
        let vis = match visibility {
            RawVisibility::Module(path, explicitness) => {
                let Unit { def_map, local, state, check, .. } = self;
                let path = &visibility_path(path, def_map.data.edition)?;
                let upstream = &mut state.upstream;
                let (krate, block) = (def_map.krate, def_map.block_id());
                let mut reads = |read: PathRead| {
                    if let PathRead::Slot(at, _) = read
                        && at.krate(db) == krate
                        && at.block(db) == block
                    {
                        held(db, check, upstream, at, import, module);
                    }
                };
                let result = def_map.resolve_path_fp_recording(
                    local.get(),
                    db,
                    ResolveMode::Other,
                    module,
                    path,
                    BuiltinShadowMode::Module,
                    None,
                    &mut reads,
                );
                if result.segment_index.is_some() {
                    return Err(VisibilityError::Unresolved);
                }
                let m = match result.resolved_def.take_types().ok_or(VisibilityError::Unresolved)? {
                    ModuleDefId::ModuleId(m) => m,
                    found => return Err(VisibilityError::NotAModule { found }),
                };
                if !Visibility::Module(m, *explicitness).is_visible_from_def_map(
                    db,
                    &self.def_map,
                    module,
                ) {
                    return Err(VisibilityError::NotAncestor { module: m });
                }
                let mut vis = Visibility::Module(m, *explicitness);

                // In block expressions, `self` normally refers to the containing non-block module, and
                // `super` to its parent (etc.). However, visibilities must only refer to a module in the
                // DefMap they're written in, so we restrict them when that happens.
                if let Visibility::Module(m, mv) = vis
                    && self.def_map.block_id() != m.block(db)
                {
                    vis = Visibility::Module(self.def_map.root, mv);
                }
                vis
            }
            RawVisibility::PubSelf(explicitness) => Visibility::Module(module, *explicitness),
            RawVisibility::Public => Visibility::Public,
            RawVisibility::PubCrate => Visibility::PubCrate(self.def_map.krate),
        };
        Ok(vis)
    }

    fn record_resolved_import(&mut self, at: u32) {
        let directive = &self.state.imports.directives[at as usize];
        let module_id = directive.op.module;
        let mut def = directive.status.namespaces();
        // clone: a `RawVisibility` is a `Copy` path and a tag.
        let raw_vis = directive.op.visibility.clone();
        let source = directive.op.source;
        // clone: an `ImportAlias` is a tag and a `Copy` name.
        let alias = directive.op.alias.clone();
        let path = directive.op.path;
        let path_kind = path.kind;
        let path_len = path.segments().len();
        let last_segment = path.segments().last().copied();
        let vis = self.resolve_visibility(module_id, &raw_vis, &path).unwrap_or(Visibility::Public);

        match source.kind {
            kind @ (ImportKind::Plain | ImportKind::TypeOnly) => {
                let name = match alias {
                    Some(ImportAlias::Alias(name)) => Some(name),
                    Some(ImportAlias::Underscore) => None,
                    None => match last_segment {
                        Some(last_segment) => Some(last_segment),
                        None => {
                            cov_mark::hit!(bogus_paths);
                            return;
                        }
                    },
                };

                if kind == ImportKind::TypeOnly {
                    def.values = None;
                    def.macros = None;
                }
                let imp = ImportOrExternCrate::Import(ImportId { use_: source.id, idx: source.use_tree });

                // `extern crate crate_name` things can be re-exported as `pub use crate_name`.
                // But they cannot be re-exported as `pub use self::crate_name`, `pub use crate::crate_name`
                // or `pub use ::crate_name`.
                //
                // This has been historically allowed, but may be not allowed in future
                // https://github.com/rust-lang/rust/issues/127909
                if let Some(def) = def.types.as_mut() {
                    let is_extern_crate_reimport_without_prefix = || {
                        let Some(ImportOrExternCrate::ExternCrate(_)) = def.import else {
                            return false;
                        };
                        if kind == ImportKind::Glob {
                            return false;
                        }
                        matches!(path_kind, PathKind::Plain | PathKind::SELF) && path_len < 2
                    };
                    if is_extern_crate_reimport_without_prefix() {
                        def.vis = vis;
                    }
                }

                if name.is_none() {
                    self.scope_mut(module_id).push_underscore_import(UnderscoreImport {
                        import: ImportId { use_: source.id, idx: source.use_tree },
                        def: def.types.map(|it| it.def),
                        vis,
                    });
                }
                self.update(module_id, &[(name, def)], vis, Some(imp));
            }
            ImportKind::Glob => {
                let glob = GlobId { use_: source.id, idx: source.use_tree };
                match def.take_types() {
                    Some(ModuleDefId::ModuleId(m)) => {
                        if source.is_prelude {
                            // Note: This dodgily overrides the injected prelude. The rustc
                            // implementation seems to work the same though.
                            cov_mark::hit!(std_prelude);
                            self.def_map.prelude = Some((m, Some(source.id)));
                            self.state.prelude = Some((m, Some(source.id)));
                            self.queues.wake_all(&self.state.imports);
                        } else if m.krate(self.db) != self.def_map.krate {
                            cov_mark::hit!(glob_across_crates);
                            // glob import from other crate => we can just import everything once
                            let scope = &m.def_map(self.db).module(self.db, m).scope(self.db);

                            // Module scoped macros is included
                            let start = self.held.len();
                            self.held.extend(
                                scope
                                    .resolutions()
                                    // only keep visible names...
                                    .map(|(n, res)| {
                                        (n, res.filter_visibility(|v| v.is_visible_from_other_crate()))
                                    })
                                    .filter(|(_, res)| !res.is_none()),
                            );
                            self.update_held(
                                module_id,
                                start,
                                vis,
                                Some(ImportOrExternCrate::Glob(glob)),
                            );
                        } else {
                            // glob import from same crate => we do an initial
                            // import, and then need to propagate any further
                            // additions
                            if m.block(self.db) == self.def_map.block_id()
                                && !held(
                                    self.db,
                                    &self.check,
                                    &mut self.state.upstream,
                                    m,
                                    &path,
                                    module_id,
                                )
                                && !self.state.upstream_globs.contains(&m)
                            {
                                self.state.upstream_globs.push(m);
                            }
                            self.pull_glob(m, module_id, vis, glob);
                            // record the glob import in case we add further items
                            let (db, def_map) = (self.db, &self.def_map);
                            // Two globs of one module share an edge. The one its names are bound
                            // through (`glob_takes_over`) takes it over, so what the source gains
                            // later is credited as when it was pulled.
                            self.state.globs.add(m, module_id, (vis, glob), |(old_vis, old_glob)| {
                                if glob_takes_over(db, def_map, glob, vis, Some(*old_glob), *old_vis) {
                                    *old_glob = glob;
                                }
                                if let Some(new_vis) = old_vis.max(db, vis, def_map) {
                                    *old_vis = new_vis;
                                }
                            });
                        }
                    }
                    Some(ModuleDefId::AdtId(AdtId::EnumId(e))) => {
                        cov_mark::hit!(glob_enum);
                        // A variant is no binding of the enum's module: a write touching the enum
                        // wakes this read (`enum_glob_wakes`).
                        crate::nameres::delta::reads::note(
                            crate::Lookup::lookup(&e, self.db).container,
                            Some(crate::nameres::delta::reads::enum_glob()),
                        );
                        // glob import from enum => just import all the variants
                        let start = self.held.len();
                        self.held.extend(e.enum_variants(self.db).variants.iter().map(
                            |(name, &(variant, shape))| {
                                // We don't need to reduce the visibility, since enum variants and their fields do not have visibility on their own.
                                let res = if shape.has_value_ns_ctor() {
                                    PerNs::both(variant.into(), variant.into(), vis, None)
                                } else {
                                    PerNs::types(variant.into(), vis, None)
                                };
                                (Some(*name), res)
                            },
                        ));
                        self.update_held(module_id, start, vis, Some(ImportOrExternCrate::Glob(glob)));
                    }
                    Some(ModuleDefId::TraitId(it)) => {
                        // glob import from trait => its associated items, as rustc's traits are
                        // modules to `use`. None where it cannot list them, which the end of
                        // collection reports. An item is no binding of the trait's module: a
                        // write touching the trait wakes this read (`enum_glob_wakes`).
                        crate::nameres::delta::reads::note(
                            crate::Lookup::lookup(&it, self.db).container,
                            Some(crate::nameres::delta::reads::trait_import()),
                        );
                        let start = self.held.len();
                        let held = &mut self.held;
                        let listed = self.def_map.for_each_trait_item_for_import(
                            self.db,
                            it,
                            false,
                            |name, item| held.push((Some(name), trait_item_per_ns(item, vis))),
                        );
                        if listed.is_err() {
                            self.held.truncate(start);
                        }
                        self.update_held(module_id, start, vis, Some(ImportOrExternCrate::Glob(glob)));
                    }
                    Some(d) => {
                        tracing::debug!("glob import from non-module/enum {:?}", d);
                    }
                    None => {
                        tracing::debug!("glob import didn't resolve as type");
                    }
                }
            }
        }
    }

    /// What a glob of `importer` takes of `source`, a module of this crate's map: every name of
    /// its scope as it is now that `importer` can see, with the glob's visibility.
    fn pull_glob(&mut self, source: ModuleId, importer: ModuleId, vis: Visibility, glob: GlobId) {
        let scope = if source.block(self.db) == self.def_map.block_id() {
            self.def_map.module(self.db, source).scope(self.db)
        } else {
            source.def_map(self.db).module(self.db, source).scope(self.db)
        };

        // Module scoped macros is included
        let start = self.held.len();
        self.held.extend(
            scope
                .resolutions()
                // only keep visible names...
                .map(|(n, res)| {
                    (
                        n,
                        res.filter_visibility(|v| {
                            v.is_visible_from_def_map(self.db, &self.def_map, importer)
                        }),
                    )
                })
                .filter(|(_, res)| !res.is_none()),
        );
        self.update_held(importer, start, vis, Some(ImportOrExternCrate::Glob(glob)));
    }

    /// At the start of a round a component carries on into: the modules of other components
    /// its members glob-import may have gained names, so each glob takes them again.
    fn repull_upstream_globs(&mut self) {
        for at in 0..self.state.upstream_globs.len() {
            let source = self.state.upstream_globs[at];
            // Pulling adds no glob edge, so the list read as it goes is the list as it was.
            let mut edge = self.state.globs.first(source);
            while edge != NONE {
                let ImporterEdge { importer, data: (vis, glob), next } = self.state.globs.edge(edge);
                edge = next;
                self.pull_glob(source, importer, vis, glob);
            }
        }
    }

    fn update(
        &mut self,
        // The module for which `resolutions` have been resolve
        module_id: ModuleId,
        resolutions: &[(Option<Name>, PerNs)],
        // Visibility this import will have
        vis: Visibility,
        import: Option<ImportOrExternCrate>,
    ) {
        let start = self.held.len();
        self.held.extend_from_slice(resolutions);
        self.update_held(module_id, start, vis, import);
    }

    /// `update` with the resolutions at `start..` of `held`, which it takes off.
    fn update_held(
        &mut self,
        module_id: ModuleId,
        start: usize,
        vis: Visibility,
        import: Option<ImportOrExternCrate>,
    ) {
        let end = self.held.len();
        let span = tracing::info_span!(
            "nameres update",
            resolutions = end - start,
            names_pushed = tracing::field::Empty,
            edges_walked = tracing::field::Empty,
        )
        .entered();
        let (names, edges) = (self.names_pushed, self.edges_walked);
        self.update_recursive(module_id, start..end, vis, import, 0);
        span.record("names_pushed", self.names_pushed - names);
        span.record("edges_walked", self.edges_walked - edges);
        self.held.truncate(start);
    }

    fn update_recursive(
        &mut self,
        // The module for which `resolutions` have been resolved.
        module_id: ModuleId,
        // Where in `held` they are.
        resolutions: std::ops::Range<usize>,
        // All resolutions are imported with this visibility; the visibilities in
        // the `PerNs` values are ignored and overwritten
        vis: Visibility,
        import: Option<ImportOrExternCrate>,
        depth: usize,
    ) {
        if depth > GLOB_RECURSION_LIMIT {
            // prevent stack overflows (but this shouldn't be possible)
            panic!("nameres: glob imports into {module_id:?} recurse past {GLOB_RECURSION_LIMIT} levels");
        }
        let mut changed = false;

        self.names_pushed += resolutions.len() as u64;
        // clone: a `Range` of two indices.
        for at in resolutions.clone() {
            let (name, res) = self.held[at];
            match name {
                Some(name) => {
                    changed |= self.push_res_and_update_glob_vis(module_id, &name, res, vis, import);
                }
                None => {
                    let tr = match res.take_types_full() {
                        Some(Item { def: ModuleDefId::TraitId(tr), vis: _, import: _ }) => tr,
                        Some(other) => {
                            tracing::debug!("non-trait `_` import of {:?}", other);
                            continue;
                        }
                        None => continue,
                    };
                    let old_vis =
                        self.def_map.module(self.db, module_id).scope(self.db).unnamed_trait_vis(tr);
                    let should_update = match old_vis {
                        None => true,
                        Some(old_vis) => {
                            let max_vis = old_vis.max(self.db, vis, &self.def_map).unwrap_or_else(|| {
                                panic!("`Tr as _` imports with unrelated visibilities {old_vis:?} and {vis:?} (trait {tr:?})");
                            });

                            if max_vis == old_vis {
                                false
                            } else {
                                cov_mark::hit!(upgrade_underscore_visibility);
                                true
                            }
                        }
                    };

                    if should_update {
                        changed = true;
                        self.scope_mut(module_id).push_unnamed_trait_through(tr, vis, import);
                    }
                }
            }
        }

        if !changed {
            return;
        }
        let first_edge = self.state.globs.first(module_id);
        if first_edge == NONE {
            return;
        }
        // A name this module already had in one namespace kept its own item there, so only
        // what the module now holds reaches the modules that glob-import it. Those go after
        // this level's, and are taken off when its importers are done.
        let held = self.held.len();
        let scope = self.def_map.module(self.db, module_id).scope(self.db);
        for at in resolutions {
            let (name, res) = self.held[at];
            let res = match name {
                None => res,
                Some(name) => {
                    let now = scope.get(&name);
                    PerNs {
                        types: res.types.filter(|it| now.types.is_some_and(|now| now.def == it.def)),
                        values: res
                            .values
                            .filter(|it| now.values.is_some_and(|now| now.def == it.def)),
                        macros: res
                            .macros
                            .filter(|it| now.macros.is_some_and(|now| now.def == it.def)),
                    }
                }
            };
            // A glob carries no unresolved name, as when it is first pulled (`pull_glob`): else
            // an importer would hold one only when it shares a unit with the import that failed.
            if res.is_none() {
                continue;
            }
            self.held.push((name, res));
        }
        let resolutions = held..self.held.len();

        // Propagating adds no glob edge, so the list read as it goes is the list as it was.
        let mut edge = first_edge;
        while edge != NONE {
            let ImporterEdge { importer, data: (glob_import_vis, glob), next } =
                self.state.globs.edge(edge);
            edge = next;
            self.edges_walked += 1;
            if !vis.is_visible_from_def_map(self.db, &self.def_map, importer) {
                continue;
            }
            let vis = glob_import_vis.min(self.db, vis, &self.def_map).unwrap_or(glob_import_vis);
            // Each name is held at the narrower of the import's visibility and its own, so the
            // importer takes only the names whose own visibility it sees, as `pull_glob` does.
            let from = self.held.len();
            // clone: a `Range` of two indices.
            for at in resolutions.clone() {
                let (name, res) = self.held[at];
                let res = res.filter_visibility(|it| it.is_visible_from_def_map(self.db, &self.def_map, importer));
                if !res.is_none() {
                    self.held.push((name, res));
                }
            }
            let visible = from..self.held.len();
            self.update_recursive(importer, visible, vis, Some(ImportOrExternCrate::Glob(glob)), depth + 1);
            self.held.truncate(from);
        }
        self.held.truncate(held);
    }

    /// Records that `glob` gives `name` of `module_id` the item `other` where another glob
    /// gave it a different one: rustc's ambiguity binding, an error where a path uses it.
    fn push_glob_ambiguity(
        &mut self,
        module_id: ModuleId,
        name: &Name,
        other: ItemInNs,
        glob: GlobId,
        vis: Visibility,
    ) {
        let scope = self.def_map.module(self.db, module_id).scope(self.db);
        let earlier = recorded_ambiguity(scope, name, other, glob);
        let vis = widest_ambiguity(self.db, &self.def_map, earlier, vis);
        let it = GlobAmbiguity { name: *name, other, glob, vis };
        self.scope_mut(module_id).push_glob_ambiguity(it);
    }

    fn push_res_and_update_glob_vis(
        &mut self,
        module_id: ModuleId,
        name: &Name,
        mut defs: PerNs,
        vis: Visibility,
        def_import_type: Option<ImportOrExternCrate>,
    ) -> bool {
        let glob = matches!(def_import_type, Some(ImportOrExternCrate::Glob(_)));
        let (db, def_map) = (self.db, &self.def_map);
        let narrowed = |it: Visibility| {
            let it = it.min(db, vis, def_map).unwrap_or(vis);
            if glob { it.root_as_pub_crate(db, def_map) } else { it }
        };
        if let Some(def) = defs.types.as_mut() {
            def.vis = narrowed(def.vis);
        }
        if let Some(def) = defs.values.as_mut() {
            def.vis = narrowed(def.vis);
        }
        if let Some(def) = defs.macros.as_mut() {
            def.vis = narrowed(def.vis);
        }
        // Whatever the scope does with them: an import that read the slot is retried, and
        // finds the same answer if nothing it reads changed.
        self.queues.wake_slot(&self.state.imports, module_id, *name, written_ns(&defs));

        // A name is unresolved only where nothing resolved it, never where every namespace was
        // handled below.
        let unresolved = defs.is_none();
        let mut changed = false;

        if let Some(ImportOrExternCrate::Glob(glob)) = def_import_type {
            let prev_defs = self.def_map.module(self.db, module_id).scope(self.db).get(name);

            // Multiple globs may import the same item and they may override visibility from
            // previously resolved globs. Handle overrides here and leave the rest to
            // `ItemScope::push_res_with_import()`.
            if let Some(def) = defs.types
                && let Some(prev_def) = prev_defs.types
            {
                if def.def == prev_def.def
                    && self.state.from_glob_import.contains_type(module_id, *name)
                    && glob_takes_over(
                        self.db,
                        &self.def_map,
                        glob,
                        def.vis,
                        prev_def.import.and_then(ImportOrExternCrate::glob),
                        prev_def.vis,
                    )
                {
                    changed = true;
                    // This import is being handled here, don't pass it down to
                    // `ItemScope::push_res_with_import()`.
                    defs.types = None;
                    self.scope_mut(module_id).rebind_glob_types(name, def, glob);
                }
                // When the source module's definition changed (e.g., due to an explicit import
                // shadowing a glob), propagate the new definition to modules that glob-import from it.
                // We check that the previous definition came from the same glob import to avoid
                // incorrectly overwriting definitions from different glob sources.
                //
                // Note this is not a perfect fix, but it makes
                // https://github.com/rust-lang/rust-analyzer/issues/19224 work for now until we
                // implement a proper glob graph
                else if def.def != prev_def.def && prev_def.import == def_import_type {
                    changed = true;
                    defs.types = None;
                    self.scope_mut(module_id).update_def_types(name, def.def, def.vis);
                } else if def.def != prev_def.def
                    && let Some(ImportOrExternCrate::Glob(prev_glob)) = prev_def.import
                {
                    if glob_position(self.db, glob) < glob_position(self.db, prev_glob) {
                        changed = true;
                        defs.types = None;
                        self.scope_mut(module_id).rebind_glob_types(name, def, glob);
                        let other = ItemInNs::Types(prev_def.def);
                        self.push_glob_ambiguity(module_id, name, other, prev_glob, prev_def.vis);
                    } else {
                        let other = ItemInNs::Types(def.def);
                        self.push_glob_ambiguity(module_id, name, other, glob, def.vis);
                    }
                }
            }

            if let Some(def) = defs.values
                && let Some(prev_def) = prev_defs.values
            {
                if def.def == prev_def.def
                    && self.state.from_glob_import.contains_value(module_id, *name)
                    && glob_takes_over(
                        self.db,
                        &self.def_map,
                        glob,
                        def.vis,
                        prev_def.import.map(ImportOrExternCrate::from).and_then(ImportOrExternCrate::glob),
                        prev_def.vis,
                    )
                {
                    changed = true;
                    defs.values = None;
                    self.scope_mut(module_id).rebind_glob_values(name, def, glob);
                } else if def.def != prev_def.def
                    && prev_def.import.map(ImportOrExternCrate::from) == def_import_type
                {
                    changed = true;
                    defs.values = None;
                    self.scope_mut(module_id).update_def_values(name, def.def, def.vis);
                } else if def.def != prev_def.def
                    && let Some(ImportOrGlob::Glob(prev_glob)) = prev_def.import
                {
                    if glob_position(self.db, glob) < glob_position(self.db, prev_glob) {
                        changed = true;
                        defs.values = None;
                        self.scope_mut(module_id).rebind_glob_values(name, def, glob);
                        let other = ItemInNs::Values(prev_def.def);
                        self.push_glob_ambiguity(module_id, name, other, prev_glob, prev_def.vis);
                    } else {
                        let other = ItemInNs::Values(def.def);
                        self.push_glob_ambiguity(module_id, name, other, glob, def.vis);
                    }
                }
            }

            if let Some(def) = defs.macros
                && let Some(prev_def) = prev_defs.macros
            {
                if def.def == prev_def.def
                    && self.state.from_glob_import.contains_macro(module_id, *name)
                    && glob_takes_over(
                        self.db,
                        &self.def_map,
                        glob,
                        def.vis,
                        prev_def.import.and_then(ImportOrExternCrate::glob),
                        prev_def.vis,
                    )
                {
                    changed = true;
                    defs.macros = None;
                    self.scope_mut(module_id).rebind_glob_macros(name, def, glob);
                } else if def.def != prev_def.def && prev_def.import == def_import_type {
                    changed = true;
                    defs.macros = None;
                    self.scope_mut(module_id).update_def_macros(name, def.def, def.vis);
                } else if def.def != prev_def.def
                    && let Some(ImportOrExternCrate::Glob(prev_glob)) = prev_def.import
                {
                    if glob_position(self.db, glob) < glob_position(self.db, prev_glob) {
                        changed = true;
                        defs.macros = None;
                        self.scope_mut(module_id).rebind_glob_macros(name, def, glob);
                        let other = ItemInNs::Macros(prev_def.def);
                        self.push_glob_ambiguity(module_id, name, other, prev_glob, prev_def.vis);
                    } else {
                        let other = ItemInNs::Macros(def.def);
                        self.push_glob_ambiguity(module_id, name, other, glob, def.vis);
                    }
                }
            }
        }

        if unresolved || !defs.is_none() {
            changed |= row_of(&mut self.def_map.modules, &mut self.touched, module_id)
                .scope_mut()
                .push_res_with_import(
                    &mut self.state.from_glob_import,
                    (module_id, *name),
                    defs,
                    def_import_type,
                );
        }

        changed
    }
}

impl<'db, A: Allocator + Clone> Sink<'db> for Unit<'db, '_, A> {
    fn db(&self) -> &'db Db {
        self.db
    }

    fn def_map(&self) -> &DefMap {
        &self.def_map
    }

    fn local_def_map(&self) -> &LocalDefMap {
        self.local.get()
    }

    fn apply(&mut self, op: DeclOp) {
        match op {
            DeclOp::Import(it) => self.state.imports.push(&mut self.queues, it),
            DeclOp::Macro(it) => self.macros.push(it),
            DeclOp::Diagnostic(diagnostic) => self.diagnostics.push(diagnostic),
            op @ (DeclOp::NotIntroducing(_)
            | DeclOp::ResolvedMacroPath { .. }
            | DeclOp::DeriveHelperUse(_)) => self.records.push(op),
            DeclOp::InitDeriveAttribute { module, adt, attr, attr_call, derive_calls } => {
                self.scope_mut(module).init_derive_attribute(adt, attr, attr_call, derive_calls);
            }
            op => self.replay(&op),
        }
    }

    fn find_unresolved_named<T>(
        &self,
        module: ModuleId,
        name: Name,
        mut f: impl FnMut(&ModPath) -> Option<T>,
    ) -> Option<T> {
        self.state.imports.unresolved_named(module, name).find_map(|it| f(&it.op.path))
    }

    /// The child's row is made by the `Init` the collection applies next, before any op of its
    /// items.
    fn child(&mut self, request: ChildRequest<'_>) -> ChildMode {
        let _ = request;
        ChildMode::Inline
    }

    fn take_macros(&mut self, into: &mut FrameVec<'_, MacroOp>) {
        into.extend(self.macros.drain(..));
    }
}

/// The scope a module's round-0 log leaves, its imports unresolved: the replay of the ops that
/// target it (and, for the crate root, its `Splice`s), from the textual scope it inherits. The
/// unit it replays on is `frame`'s, the calling query's.
pub(crate) fn local_scope(
    db: &Db,
    frame: &Frame,
    module: ModuleId,
    inherited: &[(Name, Box<[MacroId]>)],
    ops: &OpLog,
) -> BuildingScope {
    let krate = module.krate(db);
    assert!(
        module.block(db).is_none(),
        "nameres facts: a round-0 log is a crate module's, and {module:?} is a block's"
    );
    let root = queries::crate_root(db, krate);
    let Some(&DeclOp::Init { origin, visibility, parent, .. }) = ops.iter().next() else {
        panic!("nameres facts: the round-0 log of {module:?} does not start with its `Init`")
    };
    let mut rows = ModulesMap::new();
    rows.insert(module, ModuleData::new(origin, visibility, parent));
    let def_map = DefMap {
        krate,
        block: None,
        root,
        modules: ModulesSlot::Building(rows),
        prelude: None,
        macro_use_prelude: HashMap::default(),
        derive_helpers_in_scope: HashMap::default(),
        session_macros: false,
        names: uses::NameRecords::default(),
        // alloc: empty, a scope's view keeps no diagnostics
        diagnostics: Vec::new(),
        diagnostics_by_module: DiagnosticsByModule::default(),
        records: None,
        // clone: one count of the crate's shared data.
        data: queries::crate_data(db, krate).clone(),
    };
    let state = UnitState::new_in(frame);
    let mut unit = Unit::new(db, frame, def_map, Local::Borrowed(LocalDefMap::EMPTY), state, None);
    for (name, macs) in inherited {
        for &mac in macs {
            unit.scope_mut(module).define_legacy_macro(*name, mac);
        }
    }
    let is_root = module == root;
    for op in ops.iter() {
        let applies = match op {
            DeclOp::Init { .. } | DeclOp::Import(_) => false,
            DeclOp::Splice { .. } => is_root,
            op => op.target() == Some(module) && op.is_resolution_op(),
        };
        if applies {
            unit.replay(op);
        }
    }
    let Some(ModuleData { scope: ScopeSlot::Building(scope), .. }) =
        own_rows(&mut unit.def_map.modules).swap_remove(&module)
    else {
        unreachable!("a unit holds its own rows, building")
    };
    *scope
}

/// The round's `#[prelude_import]`: the index of the module that holds it. The graph puts that
/// module's component upstream of every other.
#[blaze_query::tracked(returns(copy))]
fn prelude_holder(db: &Db, round: RoundKey) -> Option<u32> {
    let shape = queries::import_shape(db, round);
    let at = shape.modules.iter().position(|it| it.imports.iter().any(|import| import.is_prelude))?;
    Some(at as u32)
}

/// The std prelude the round's `#[prelude_import]` resolved to, if one did: what every other
/// component installs, from the holder's state at the last round its component ran.
/// Backdates while it stays.
#[blaze_query::tracked(returns(copy))]
pub(super) fn round_prelude(db: &Db, round: RoundKey) -> Option<(ModuleId, Option<UseId>)> {
    let holder = prelude_holder(db, round)?;
    let krate = round.krate(db);
    let modules = queries::round_modules(db, round);
    let ran = queries::ran_by(db, round, holder);
    // A component that does not run has the members it had when it last ran.
    let partition = queries::partition(db, RoundKey::new(db, krate, ran));
    let anchor = modules.order[partition.anchor(partition.of(holder)) as usize];
    queries::scc_state(db, SccKey::new(db, anchor, krate, ran)).prelude
}

/// Whether every component runs at the round: round 0, the final round, and a round whose
/// crate env or `#[prelude_import]` module is not the round before's.
fn all_run(db: &Db, round: RoundKey) -> bool {
    let (krate, r) = (round.krate(db), round.round(db));
    if r == 0 || queries::round_kind(db, round) == RoundKind::Final {
        return true;
    }
    let before = RoundKey::new(db, krate, r - 1);
    queries::round_env(db, round) != queries::round_env(db, before)
        || prelude_holder(db, round) != prelude_holder(db, before)
}

/// Whether component `component` of the round runs for what its members are: one gains ops or
/// is new, or its members are not those it had when it last ran. Not whether what it read
/// changes: `round_active` adds that.
fn members_change(db: &Db, round: RoundKey, component: u32) -> bool {
    let krate = round.krate(db);
    let modules = queries::round_modules(db, round);
    let partition = queries::partition(db, round);
    let members = partition.members(component);
    if members.iter().any(|&at| modules.gains(at) || modules.ran(at).is_none()) {
        return true;
    }
    let anchor = partition.anchor(component);
    let Some(ran) = modules.ran(anchor) else { return true };
    let then = queries::partition(db, RoundKey::new(db, krate, ran));
    let at_then = then.of(anchor);
    then.anchor(at_then) != anchor || then.members(at_then) != members
}

/// Whether component `component` of the round runs (`queries::RoundActive`). The component of
/// the `#[prelude_import]` is upstream of every other and reads none, so whether it runs is
/// known without `round_active`, which reads its state when it runs.
pub(crate) fn component_runs(db: &Db, round: RoundKey, component: u32) -> bool {
    let partition = queries::partition(db, round);
    match prelude_holder(db, round) {
        Some(holder) if partition.of(holder) == component => {
            all_run(db, round) || members_change(db, round, component)
        }
        _ => queries::round_active(db, round).runs(component),
    }
}

/// Which components of the round run (`nameres-core.md` §9), upstream first: every one when
/// [`all_run`] or the std prelude the holder resolves changes; else each one whose members
/// change ([`members_change`]) or that read a module of one that runs, by the `upstream` of
/// its state at the round it last ran.
pub(crate) fn round_active(db: &Db, round: RoundKey) -> RoundActive {
    let (krate, r) = (round.krate(db), round.round(db));
    let modules = queries::round_modules(db, round);
    let partition = queries::partition(db, round);
    let n = partition.len();
    if all_run(db, round) {
        return RoundActive::all(n);
    }
    let holder = prelude_holder(db, round).map(|at| partition.of(at));
    if let Some(component) = holder
        && members_change(db, round, component)
        && round_prelude(db, round) != round_prelude(db, RoundKey::new(db, krate, r - 1))
    {
        return RoundActive::all(n);
    }

    // alloc: the query's value, one word per 64 components.
    let mut bits = vec![0u64; (n as usize).div_ceil(64)];
    let runs = |bits: &[u64], c: u32| bits[(c / 64) as usize] & (1 << (c % 64)) != 0;
    for component in 0..n {
        let on = match holder == Some(component) {
            true => members_change(db, round, component),
            false => members_change(db, round, component) || {
                let anchor = partition.anchor(component);
                let ran = modules.ran(anchor).expect("a component whose members did not change ran before");
                let anchor = modules.order[anchor as usize];
                let then = queries::scc_state(db, SccKey::new(db, anchor, krate, ran));
                then.upstream.iter().any(|&(module, _)| {
                    let Some(at) = modules.at(module) else {
                        panic!("nameres facts: {anchor:?} read {module:?} at round {ran}, which is not a module of round {r}")
                    };
                    let upstream = partition.of(at);
                    upstream >= component || runs(&bits, upstream)
                })
            },
        };
        if on {
            bits[(component / 64) as usize] |= 1 << (component % 64);
        }
    }
    RoundActive { bits: bits.into_boxed_slice() }
}

/// One component's resolution at one round (`nameres-core.md` §5). Only a component the round
/// runs has one (`round_active`); its state carries on from the last round it ran at.
pub(crate) fn scc_state(db: &Db, frame: &Frame, key: SccKey) -> SccState {
    let (anchor, krate, r) = (key.anchor(db), key.krate(db), key.round(db));
    let round = RoundKey::new(db, krate, r);
    let modules = queries::round_modules(db, round);
    let partition = queries::partition(db, round);
    let Some(&anchor_at) = modules.index.get(&anchor) else {
        panic!("nameres facts: the anchor {anchor:?} is not a module of round {r} of {krate:?}")
    };
    let component = partition.of(anchor_at);
    assert_eq!(
        partition.anchor(component),
        anchor_at,
        "nameres facts: {anchor:?} is not the anchor of its component at round {r}"
    );
    assert!(
        component_runs(db, round, component),
        "nameres facts: scc_state of {anchor:?} at round {r}, a component the round does not run"
    );
    let member_at = partition.members(component);
    let kind = queries::round_kind(db, round);
    let env = queries::round_env(db, round);
    let check = ReadCheck { partition, index: &modules.index, component };

    let (holds_prelude, prelude) = installed_prelude(db, round, member_at);

    // The same component at the last round it ran at: its state carries on.
    let prev = match modules.ran(anchor_at) {
        None => None,
        Some(ran) => {
            let prev_round = RoundKey::new(db, krate, ran);
            let prev_partition = queries::partition(db, prev_round);
            let prev_component = prev_partition.of(anchor_at);
            let same = prev_partition.anchor(prev_component) == anchor_at
                && prev_partition.members(prev_component) == member_at;
            same.then(|| (prev_round, queries::scc_state(db, SccKey::new(db, anchor, krate, ran))))
        }
    };

    if let Some((prev_round, prev)) = prev
        && kind != RoundKind::Final
        && env == queries::round_env(db, prev_round)
        && installed_prelude(db, prev_round, member_at) == (holds_prelude, prelude)
        && member_at.iter().all(|&at| {
            let module = modules.order[at as usize];
            !modules.gains(at) || queries::module_round(db, FactKey::new(db, module, krate, r)).ops.is_empty()
        })
        && prev.upstream.iter().all(|&(module, handle)| {
            check.may_read(module) && queries::fact_handle(db, fact_key_of(db, round, module)) == handle
        })
    {
        return SccState {
            round: prev.round,
            // clone: the value owns its lists; one copy of each per unchanged round.
            members: prev.members.clone(),
            // clone: as above.
            handles: prev.handles.clone(),
            prelude: prev.prelude,
            // clone: as above.
            upstream: prev.upstream.clone(),
            imports: Box::default(),
            // clone: one count; the state is the carried one, unchanged.
            state: prev.state.clone(),
        };
    }

    let mut members = Vec::with_capacity_in(member_at.len(), frame);
    members.extend(member_at.iter().map(|&at| modules.order[at as usize]));
    let mut unit = match prev {
        Some((_, prev)) => {
            for &module in &prev.state.upstream {
                if !check.may_read(module) {
                    panic!(
                        "nameres facts: {anchor:?} carries a read of {module:?} into round {r}, which the partition does not put upstream"
                    );
                }
            }
            // clone: once per round a component changes; the carried state stays the round
            // before's.
            let state = UnitState::clone(&prev.state);
            let mut unit = Unit::component(db, frame, round, env, Some(check), state, prelude);
            for &module in &members {
                unit.replay_round(module, r);
            }
            unit.repull_upstream_globs();
            unit.queues.wake_all(&unit.state.imports);
            unit
        }
        None => {
            let state = UnitState::default();
            let mut unit = Unit::component(db, frame, round, env, Some(check), state, prelude);
            unit.replay_fresh(&members, r);
            unit
        }
    };
    unit.set_unexpanded(modules.pending != 0);
    unit.resolve_imports();

    let mut imports: Box<[(ImportOp, Option<PerNs>)]> = Box::default();
    if kind == RoundKind::Final {
        unit.record_unresolved();
        // clone: the final round's imports, once, for `finish`'s diagnostics and `NameRecords`.
        imports = unit.import_outcomes().map(|(import, status)| (import.clone(), status)).collect();
        let root = unit.def_map.root;
        if unit.def_map.data.proc_macro && members.contains(&root) {
            // A crate exporting procedural macros is not allowed to export anything else.
            //
            // Additionally, while the proc macro entry points must be `pub`, they are not publicly
            // exported in type/value namespace. This function reduces the visibility of all items
            // in the crate root that aren't proc macros.
            unit.scope_mut(root).censor_non_proc_macros(krate);
        }
        // The old `DefMap::shrink_to_fit` at the end of collection: these scopes are the
        // crate's, handed over below, so they never reach it.
        for &module in &members {
            if !unit.def_map.module(db, module).scope(db).glob_ambiguities().is_empty() {
                unit.scope_mut(module).settle_glob_ambiguities();
            }
        }
    }

    // Each member's handle: the one it had the round before when its scope is the same.
    let mut handles = Vec::with_capacity_in(members.len(), frame);
    let mut all_carried = prev.is_some();
    for (at, &module) in members.iter().enumerate() {
        let before = match prev {
            Some((_, prev)) => Some(prev.handles[at]),
            None if modules.ran(member_at[at]).is_some() => {
                let before = RoundKey::new(db, krate, r - 1);
                Some(queries::fact_handle(db, queries::fact_key(db, before, member_at[at])))
            }
            None => None,
        };
        let data = unit.def_map.module(db, module);
        let ScopeSlot::Building(building) = &data.scope else {
            unreachable!("a unit's rows are building")
        };
        let same = |id: blaze_query::db::Id| {
            let scope = <ModuleScope<'_> as blaze_query::db::FromId>::from_id(id);
            *scope.scope(db) == building.scope && *scope.children(db) == building.children
        };
        let handle = match before {
            Some(id) if prev.is_some() && !unit.touched.contains(&module) => id,
            Some(id) if same(id) => id,
            _ => {
                all_carried = false;
                let (scope, children) = (&building.scope, &building.children);
                // clone: the state carries the scope on; the database holds this copy, once per change.
                let scope = ModuleScope::new(db, module, scope.clone(), children.clone());
                blaze_query::db::AsId::as_id(&scope)
            }
        };
        handles.push(handle);
    }
    if prev.is_none() {
        all_carried = false;
    }

    let state = unit.into_state();
    let handle_of = |module| queries::fact_handle(db, fact_key_of(db, round, module));
    let mut upstream = Vec::with_capacity_in(state.upstream.len(), frame);
    upstream.extend(state.upstream.iter().map(|&module| (module, handle_of(module))));
    upstream.sort_unstable_by_key(|&(module, _)| check.at(module));
    let prelude = if holds_prelude { state.prelude } else { None };

    SccState {
        round: match (prev, all_carried) {
            (Some((_, prev)), true) => prev.round,
            _ => r,
        },
        // alloc: the value's lists, at exact size, once per run that is not the quick path
        members: Box::from(&members[..]),
        // alloc: as `members`
        handles: Box::from(&handles[..]),
        prelude,
        // alloc: as `members`
        upstream: Box::from(&upstream[..]),
        imports,
        // alloc: the state the next round carries on from, once per run that is not the quick path.
        state: Arc::new(state),
    }
}

/// `queries::fact_key` of `module` of the round.
fn fact_key_of(db: &Db, round: RoundKey, module: ModuleId) -> FactKey {
    let Some(at) = queries::round_modules(db, round).at(module) else {
        panic!("nameres facts: {module:?} is not a module of round {}", round.round(db))
    };
    queries::fact_key(db, round, at)
}

/// Whether the component of `members` (ascending module indices of `round`) holds the round's
/// `#[prelude_import]`, and if it does not, the std prelude that import resolved to.
fn installed_prelude(
    db: &Db,
    round: RoundKey,
    members: &[u32],
) -> (bool, Option<(ModuleId, Option<UseId>)>) {
    match prelude_holder(db, round) {
        None => (false, None),
        Some(at) if members.binary_search(&at).is_ok() => (true, None),
        Some(_) => (false, round_prelude(db, round)),
    }
}

impl<A: Allocator + Clone> Unit<'_, '_, A> {
    /// A component that merged or is new at round `r`: rounds `0..=r` of each member, round by
    /// round, in module order.
    pub(super) fn replay_fresh(&mut self, members: &[ModuleId], r: u32) {
        let db = self.db;
        let krate = self.def_map.krate;
        let rows = queries::round_modules(db, RoundKey::new(db, krate, r));
        for round in 0..=r {
            for &module in members {
                let since = rows.row(module).map_or(0, |row| row.since);
                if since <= round {
                    self.replay_round(module, round);
                }
            }
        }
    }
}

/// Where `key`'s module is: its component's state at the round, and its place among the
/// members. A module not in the round stops, naming it, and so does one whose component the
/// round does not run (`scc_state`): its readers name it by `queries::fact_key`.
fn member_state(db: &Db, key: FactKey) -> (&SccState, usize) {
    let (module, krate, r) = (key.module(db), key.krate(db), key.round(db));
    let round = RoundKey::new(db, krate, r);
    let modules = queries::round_modules(db, round);
    let Some(&at) = modules.index.get(&module) else {
        panic!("nameres facts: {module:?} is not a module of round {r} of {krate:?}")
    };
    let partition = queries::partition(db, round);
    let component = partition.of(at);
    let anchor = modules.order[partition.anchor(component) as usize];
    let state = queries::scc_state(db, SccKey::new(db, anchor, krate, r));
    let Ok(position) = partition.members(component).binary_search(&at) else {
        unreachable!("a module is one of its component's members")
    };
    (state, position)
}

/// The module's `ModuleScope` after the round's import fixed point.
pub(crate) fn fact_handle(db: &Db, key: FactKey) -> blaze_query::db::Id {
    let (state, at) = member_state(db, key);
    state.handles[at]
}

/// The module's plain imports still unresolved after the round's fixed point, by the name each
/// binds, in import order.
pub(crate) fn unresolved_named(db: &Db, key: FactKey) -> Box<[(Name, ModPath)]> {
    let module = key.module(db);
    let (state, _) = member_state(db, key);
    let imports = &state.state.imports;
    let named = || {
        imports
            .directives
            .iter()
            .zip(&imports.list)
            .filter(|(it, list)| {
                **list == ImportList::Unresolved
                    && it.op.module == module
                    && it.op.source.kind == ImportKind::Plain
            })
            .filter_map(|(it, _)| Some((it.op.bound_name()?, &it.op.path)))
    };
    // alloc: the query's value, at exact size, once per module and round
    let mut out = Vec::with_capacity(named().count());
    // clone: each path into the query's value, once per module and round
    out.extend(named().map(|(name, path)| (name, path.clone())));
    out.into_boxed_slice()
}
