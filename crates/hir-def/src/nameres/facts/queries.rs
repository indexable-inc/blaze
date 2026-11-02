//! The queries of the per-module resolver (`docs/v2/plan/lanes/frontend/nameres.md` §3,
//! `nameres-core.md` §3). Every key is content (crate, round, module, anchor module), never a
//! dense index, and every key's `Debug` names its module. Each query here is a thin entry; the
//! work is in `collect`, `unit`, `macros` and `driver`.

use base_db::{Crate, Db};
use blaze_hash::HashMap;
use blaze_mem::{Tag, frame};
use hir_expand::{mod_path::ModPath, name::Name};
use triomphe::Arc;

use crate::{
    MacroId, ModuleId, UseId,
    item_tree::{ModItemId, TreeId},
    nameres::{DefMapCrateData, ModuleData, ModuleOrigin, mod_resolution::ModDir},
    per_ns::PerNs,
    visibility::Visibility,
};

use super::{
    collect, driver,
    env::{CollectEnv, CrateEnv, SideTables},
    graph::{ImportShape, Partition},
    macros,
    ops::{DeclOp, ImportOp, Inherited, MacroOp, OpLog},
    unit::{self, UnitState},
};

// Keys.

/// Which items a module's round-0 collection walks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DeclItems {
    /// The top-level items of the key's tree (a file module, the crate root).
    Top,
    /// The items of the inline `mod` item `ModItemId` of the key's tree.
    Inline(ModItemId),
    /// None: a module declared but not collected (its file not found, or past the `mod`
    /// depth limit). Its log is its `Init`.
    Nothing,
}

/// A module as it is declared at round 0: everything its collection reads besides item trees
/// and other crates.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Decl {
    pub(crate) krate: Crate,
    pub(crate) module: ModuleId,
    pub(crate) origin: ModuleOrigin,
    pub(crate) visibility: Visibility,
    pub(crate) parent: Option<ModuleId>,
    pub(crate) tree: TreeId,
    pub(crate) items: DeclItems,
    pub(crate) mod_dir: ModDir,
    pub(crate) macro_depth: u32,
    /// The parent's textual (`macro_rules!`) scope where the module is declared.
    pub(crate) inherited: Inherited,
    pub(crate) env: EnvKey,
}

#[blaze_query::interned(debug, unsafe(no_lifetime))]
pub(crate) struct DeclKey {
    #[returns(ref)]
    pub(crate) decl: Decl,
}

#[blaze_query::interned(debug, unsafe(no_lifetime))]
pub(crate) struct EnvKey {
    #[returns(ref)]
    pub(crate) env: CollectEnv,
}

#[blaze_query::interned(debug, unsafe(no_lifetime))]
pub(crate) struct CrateEnvKey {
    #[returns(ref)]
    pub(crate) env: CrateEnv,
}

/// A round of a crate's collection.
#[blaze_query::interned(debug, unsafe(no_lifetime))]
pub(crate) struct RoundKey {
    #[returns(copy)]
    pub(crate) krate: Crate,
    #[returns(copy)]
    pub(crate) round: u32,
}

/// A module at a round: `fact_handle`, `module_round`, `macro_step`, `unresolved_named`. Only
/// a round that can change the module makes one: readers name an idle module's facts by the
/// round they last changed at (`fact_key`).
#[blaze_query::interned(debug, unsafe(no_lifetime))]
pub(crate) struct FactKey {
    #[returns(copy)]
    pub(crate) module: ModuleId,
    #[returns(copy)]
    pub(crate) krate: Crate,
    #[returns(copy)]
    pub(crate) round: u32,
}

/// A component of a round's import graph, by the smallest module index of its members (its
/// anchor, `Partition::anchor`), named by the module.
#[blaze_query::interned(debug, unsafe(no_lifetime))]
pub(crate) struct SccKey {
    #[returns(copy)]
    pub(crate) anchor: ModuleId,
    #[returns(copy)]
    pub(crate) krate: Crate,
    #[returns(copy)]
    pub(crate) round: u32,
}

/// Which modules a view reads through `row`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Rows {
    /// Every module of a round, each scope `ScopeRef::Fact` of that round.
    Round(RoundKey),
    /// What the round-0 collection of the module of `DeclKey` sees: its ancestors as its env
    /// holds them, every other module declared before it as its round-0 declarations.
    Decl(DeclKey),
}

#[blaze_query::interned(debug, unsafe(no_lifetime))]
pub(crate) struct RowsKey {
    #[returns(copy)]
    pub(crate) rows: Rows,
}

/// What a `ScopeSlot::Fact` holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Scope {
    /// The module's scope after round `FactKey::round`'s import fixed point.
    Fact(FactKey),
    /// The module's round-0 scope as its own collection left it.
    Decl(DeclKey),
}

#[blaze_query::interned(debug, unsafe(no_lifetime))]
pub(crate) struct ScopeRef {
    #[returns(copy)]
    pub(crate) scope: Scope,
}

/// What a round does after its import fixed point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RoundKind {
    Normal,
    /// Eager calls whose arguments name a macro that does not resolve are expanded with that
    /// error (the old `expand_eager_with_unresolved_args`).
    EagerWithErrors,
    /// The reseed of the first unresolved attribute: of `module`, at `index` of its pending
    /// directives (the old `reseed_with_unresolved_attribute`).
    Reseed { module: ModuleId, index: u32 },
    /// After the last round: units record their still-unresolved imports and the root of a
    /// proc-macro crate is censored (the old `collect`'s end); no macro step runs.
    Final,
}

// Values.

/// A module's round-0 collection.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ModuleDecls {
    pub(crate) ops: OpLog,
    /// The children it declares, in declaration order; each has its own `module_decls`.
    pub(crate) children: Box<[DeclKey]>,
    /// Its textual scope after collection, every definition of each name.
    pub(crate) legacy: Inherited,
    /// Its tables after collection: where its first macro step starts.
    pub(crate) side: Arc<SideTables>,
    /// The files other than its modules' own that this log reads, whose write changes it with
    /// no binding changed: a `mod`'s file left out for its `#![cfg]`, and the file of each
    /// declarative macro it expanded.
    pub(crate) other_files: Box<[span::FileId]>,
}

/// The ops a module gains at a round `r >= 1`: from its own macro step of `r - 1`, the step that
/// declared it (its parent's), and, for the crate root, every step's root ops, in module order.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ModuleRound {
    pub(crate) ops: OpLog,
}

/// One module of a round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModuleRow {
    pub(crate) origin: ModuleOrigin,
    pub(crate) visibility: Visibility,
    pub(crate) parent: Option<ModuleId>,
    /// The round it was declared at.
    pub(crate) since: u32,
    /// Its round-0 key, for one declared at round 0.
    pub(crate) decl: Option<DeclKey>,
}

/// A round's module column that names no round (`RoundModules::last_step`, `ran`, `since`).
pub(crate) const NO_ROUND: u32 = u32::MAX;

/// Every module of a round, in the round's module order: round 0's DFS of `module_decls`,
/// then each later round's new modules in the order their steps declared them. With them, what
/// the round can change of each, known before its import fixed point (`nameres-core.md` §9):
/// whose macro step runs and who gains ops (`active`), and where the rest stand.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RoundModules {
    pub(crate) order: Box<[ModuleId]>,
    pub(crate) rows: Box<[ModuleRow]>,
    pub(crate) index: HashMap<ModuleId, u32>,
    /// Per module: [`RoundModules::STEP`], [`RoundModules::GAINS`], [`RoundModules::PENDING`].
    pub(crate) flags: Box<[u8]>,
    /// The modules with `STEP` or `GAINS`, ascending: those with a directive this round or ops
    /// to replay. Every other module's component runs only if what it read changes this round
    /// (`round_active`).
    pub(crate) active: Box<[u32]>,
    /// Per module, the round of its last macro step before this one: the step whose directives
    /// and tables it carries ([`NO_ROUND`]: it has not stepped).
    pub(crate) last_step: Box<[u32]>,
    /// Per module, the last round before this one at which its component ran: where its
    /// `scc_state` is ([`NO_ROUND`]: declared at this round).
    pub(crate) ran: Box<[u32]>,
    /// Per module, the last round before this one at which its facts changed: `fact_handle` of
    /// that round names them until its component runs again ([`NO_ROUND`]: declared at this
    /// round).
    pub(crate) since: Box<[u32]>,
    /// How many modules have [`RoundModules::PENDING`]: while any does, an import bound
    /// through a glob stays undetermined (`Unit::set_unexpanded`).
    pub(crate) pending: u32,
}

impl RoundModules {
    /// Its macro step runs this round: it has a directive and the round resolves, or the round
    /// reseeds it.
    pub(crate) const STEP: u8 = 1;
    /// `module_round` may hand it ops this round: the round's first module ops (round 0), its own
    /// step of the round before made ops in it, it was declared by one, or it is the crate root
    /// and another module's step made ops in it.
    pub(crate) const GAINS: u8 = 2;
    /// It has macro directives left as the round starts.
    pub(crate) const PENDING: u8 = 4;

    pub(crate) fn row(&self, module: ModuleId) -> Option<&ModuleRow> {
        self.index.get(&module).map(|&at| &self.rows[at as usize])
    }

    pub(crate) fn at(&self, module: ModuleId) -> Option<u32> {
        self.index.get(&module).copied()
    }

    pub(crate) fn steps(&self, at: u32) -> bool {
        self.flags[at as usize] & Self::STEP != 0
    }

    pub(crate) fn gains(&self, at: u32) -> bool {
        self.flags[at as usize] & Self::GAINS != 0
    }

    pub(crate) fn has_pending(&self, at: u32) -> bool {
        self.flags[at as usize] & Self::PENDING != 0
    }

    /// The modules whose macro step runs, ascending.
    pub(crate) fn stepping(&self) -> impl Iterator<Item = u32> + '_ {
        self.active.iter().copied().filter(|&at| self.steps(at))
    }

    pub(crate) fn last_step(&self, at: u32) -> Option<u32> {
        some_round(self.last_step[at as usize])
    }

    pub(crate) fn ran(&self, at: u32) -> Option<u32> {
        some_round(self.ran[at as usize])
    }

    pub(crate) fn since(&self, at: u32) -> Option<u32> {
        some_round(self.since[at as usize])
    }
}

fn some_round(round: u32) -> Option<u32> {
    (round != NO_ROUND).then_some(round)
}

/// Which components of a round's partition run at the round (`nameres-core.md` §9): every one
/// at round 0 and at the final round; else those with a member that gains ops, those whose
/// members changed since they last ran, and those that read a module of one that runs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RoundActive {
    /// One bit per component.
    pub(crate) bits: Box<[u64]>,
}

impl RoundActive {
    pub(crate) fn all(components: u32) -> RoundActive {
        // alloc: the query's value, one word per 64 components
        let mut bits = vec![u64::MAX; (components as usize).div_ceil(64)];
        if components % 64 != 0
            && let Some(last) = bits.last_mut()
        {
            *last = (1 << (components % 64)) - 1;
        }
        RoundActive { bits: bits.into_boxed_slice() }
    }

    pub(crate) fn runs(&self, component: u32) -> bool {
        self.bits[(component / 64) as usize] & (1 << (component % 64)) != 0
    }
}

/// One component's resolution at one round: its members' scopes handed to the database, and
/// the state the next round carries on from.
#[derive(Debug)]
pub(crate) struct SccState {
    /// The round whose `ModuleScope`s these are: this one, or the last one at which the
    /// component changed (`latest`).
    pub(crate) round: u32,
    /// Members, ascending by module index of the round.
    pub(crate) members: Box<[ModuleId]>,
    /// Each member's `ModuleScope` (as `blaze_query::db::Id`), in `members` order.
    pub(crate) handles: Box<[blaze_query::db::Id]>,
    /// The `#[prelude_import]` this component resolved, if it holds it.
    pub(crate) prelude: Option<(ModuleId, Option<UseId>)>,
    /// Every module of another component the state read (a path's `PathRead::Slot`, a glob's
    /// source), with the `ModuleScope` it read, ascending by module: the next round the
    /// component runs at keeps this state when its members gain no op and each of these is
    /// still the module's `fact_handle` (`fact_key`); a round at which none of these modules'
    /// components runs and no member gains an op does not run the component (`round_active`).
    pub(crate) upstream: Box<[(ModuleId, blaze_query::db::Id)]>,
    /// At a `RoundKind::Final` round, every import of the members in import order with what it
    /// resolved to (`PartialResolvedImport::namespaces`; `None` while still unresolved), for
    /// `finish`'s diagnostics and `NameRecords`; empty at any other round.
    pub(crate) imports: Box<[(ImportOp, Option<PerNs>)]>,
    pub(crate) state: Arc<UnitState>,
}

/// Never equal to another execution's: a rerun is always a change to the next round, which
/// carries `state` on; `fact_handle` is what backdates.
impl PartialEq for SccState {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state) && self.handles == other.handles
    }
}
impl Eq for SccState {}

/// One module's macro resolution at one round.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MacroStep {
    /// The ops of round `r + 1` it made, of any target (the module, the crate root, the
    /// modules it declared), in order.
    pub(crate) ops: OpLog,
    /// Directives left for the next round, in order.
    pub(crate) pending: Box<[MacroOp]>,
    /// The module's tables after the step.
    pub(crate) side: Arc<SideTables>,
    /// The modules it declared, in order, with their rows.
    pub(crate) new_modules: Box<[(ModuleId, ModuleRow)]>,
    /// Whether a call resolved (the old `resolve_macros` returning `No`).
    pub(crate) progress: bool,
    /// Whether an eager call was left for the error round.
    pub(crate) deferred_eager: bool,
}

/// The fold of a round's macro steps that picks the next round's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RoundOutcome {
    pub(crate) progress: bool,
    pub(crate) deferred_eager: bool,
    /// The first unresolved attribute directive, in module order then position.
    pub(crate) first_attr: Option<(ModuleId, u32)>,
}

// Queries.

/// The crate's root module id. Created here so every query of the crate shares it.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn crate_root(db: &Db, krate: Crate) -> ModuleId {
    // This query creates it with the same fields every time, so the id is the same for as long
    // as the crate is.
    crate::ModuleIdLt::new(db, krate, None, None, Name::new_symbol_root(intern::sym::__empty))
        .to_static()
}

/// The id of the module `name` in `parent`, in `krate`'s def map or `block`'s. Created here, keyed
/// by what names the module, so every collection that finds it shares it: a def-map query's,
/// and the name-resolution maintainer's, which runs in no query and so can make no tracked
/// struct itself. Its interned arguments are never collected: the query database frees no id,
/// so the module it made keeps its id for the published scopes of other crates that hold it.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn child_module(
    db: &Db,
    krate: Crate,
    block: Option<crate::BlockId>,
    parent: ModuleId,
    name: Name,
) -> ModuleId {
    crate::ModuleIdLt::new(db, krate, block, Some(parent), name).to_static()
}

/// The crate attributes of the root file (`seed_with_top_level`'s first part).
#[blaze_query::tracked(returns(ref))]
pub(crate) fn crate_data(db: &Db, krate: Crate) -> Arc<DefMapCrateData> {
    collect::crate_data(db, krate)
}

/// The crate root's round-0 key: the extern prelude of the crate's dependencies.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn root_decl(db: &Db, krate: Crate) -> DeclKey {
    frame::task(Tag::Nameres, |frame| collect::root_decl(db, frame, krate))
}

/// One module's round-0 declarations.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn module_decls(db: &Db, key: DeclKey) -> ModuleDecls {
    frame::task(Tag::Nameres, |frame| collect::module_decls(db, frame, key))
}

/// The module's `macro_rules!` after collection, the last definition of each name: what a
/// `#[macro_use] mod` brings into its parent. Backdates unless they change.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn legacy_exports(db: &Db, key: DeclKey) -> Box<[(Name, MacroId)]> {
    let legacy = &module_decls(db, key).legacy;
    let exports = || legacy.iter().filter_map(|(name, macs)| Some((*name, *macs.last()?)));
    // alloc: the query's value, at exact size, once per execution
    let mut out = Vec::with_capacity(exports().count());
    out.extend(exports());
    out.into_boxed_slice()
}

/// The ops the module and its round-0 descendants make in the crate root (`#[macro_export]`),
/// in DFS order. Backdates unless they change. Asked of the root's children (the firewall
/// between the root's collection and their subtrees) and of each descendant that has children
/// of its own; a leaf's ops are read off its log in place, and only when its log writes the
/// root at all (`OpLog::writes_elsewhere`).
#[blaze_query::tracked(returns(ref))]
pub(crate) fn subtree_macro_exports(db: &Db, key: DeclKey) -> Box<[DeclOp]> {
    let root = crate_root(db, key.decl(db).krate);
    let ops = &module_decls(db, key).ops;
    if !ops.writes_elsewhere() && !ops.has_children() {
        return Box::default();
    }
    frame::task(Tag::Nameres, |frame| {
        let mut out = Vec::new_in(frame);
        // In log order: a later `#[macro_export]` of a name replaces an earlier one in the root.
        for op in ops.iter() {
            match op {
                DeclOp::Splice { key: child, .. } => {
                    let child_ops = &module_decls(db, *child).ops;
                    if child_ops.has_children() {
                        let exports = subtree_macro_exports(db, *child).iter();
                        out.extend(exports.filter_map(|it| it.clone_root_op(root)));
                    } else if child_ops.writes_elsewhere() {
                        out.extend(child_ops.iter().filter_map(|it| it.clone_root_op(root)));
                    }
                }
                op => out.extend(op.clone_root_op(root)),
            }
        }
        // alloc: the query's value, at exact size, once per execution
        out.into_iter().collect()
    })
}

/// The key of `child`, a round-0 child of the module of `parent`: a look-up in the children of
/// `parent`'s `module_decls`, no query of its own.
pub(crate) fn child_key(db: &Db, parent: DeclKey, child: ModuleId) -> Option<DeclKey> {
    module_decls(db, parent).children.iter().copied().find(|it| it.decl(db).module == child)
}

/// The module of `key`'s round-0 scope as a `ModuleScope`, for `Scope::Decl` reads.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn decl_handle(db: &Db, key: DeclKey) -> blaze_query::db::Id {
    let decl = key.decl(db);
    let ops = &module_decls(db, key).ops;
    let scope = frame::task(Tag::Nameres, |frame| unit::local_scope(db, frame, decl.module, &decl.inherited, ops));
    let scope = crate::nameres::ModuleScope::new(db, decl.module, scope.scope, scope.children);
    blaze_query::db::AsId::as_id(&scope)
}

/// Every module of the round, in order.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn round_modules(db: &Db, round: RoundKey) -> RoundModules {
    frame::task(Tag::Nameres, |frame| driver::round_modules(db, frame, round))
}

/// Which components of the round's partition run at the round.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn round_active(db: &Db, round: RoundKey) -> RoundActive {
    unit::round_active(db, round)
}

/// Whether the component of the module at `at` of the round runs at the round.
pub(crate) fn runs(db: &Db, round: RoundKey, at: u32) -> bool {
    unit::component_runs(db, round, partition(db, round).of(at))
}

/// The key that names the facts of the module at `at` of the round: the round's own while its
/// component runs, else that of the round its facts last changed at, whose memo made them.
pub(crate) fn fact_key(db: &Db, round: RoundKey, at: u32) -> FactKey {
    let (krate, r) = (round.krate(db), round.round(db));
    let modules = round_modules(db, round);
    let module = modules.order[at as usize];
    let t = match runs(db, round, at) {
        true => r,
        false => modules.since(at).unwrap_or_else(|| {
            panic!("nameres facts: {module:?}, new at round {r}, is in a component that does not run")
        }),
    };
    FactKey::new(db, module, krate, t)
}

/// The last round up to `round` at which the component of the module at `at` ran: where its
/// state is.
pub(crate) fn ran_by(db: &Db, round: RoundKey, at: u32) -> u32 {
    let r = round.round(db);
    if runs(db, round, at) {
        return r;
    }
    round_modules(db, round).ran(at).unwrap_or_else(|| {
        panic!("nameres facts: module {at} of round {r} is new and in a component that does not run")
    })
}

/// What the module gains at a round `>= 1`.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn module_round(db: &Db, key: FactKey) -> ModuleRound {
    frame::task(Tag::Nameres, |frame| driver::module_round(db, frame, key))
}

/// The round's import graph, for `graph::partition`. Backdates unless an import, a `mod` or
/// an `extern crate self` changes.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn import_shape(db: &Db, round: RoundKey) -> ImportShape {
    frame::task(Tag::Nameres, |frame| driver::import_shape(db, frame, round))
}

#[blaze_query::tracked(returns(ref))]
pub(crate) fn partition(db: &Db, round: RoundKey) -> Partition {
    let shape = import_shape(db, round);
    frame::task(Tag::Nameres, |frame| super::graph::partition(frame, shape))
}

/// The crate env the round's units and steps start from: the root's crate-level ops of every
/// round before, and the injected std prelude.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn round_env(db: &Db, round: RoundKey) -> CrateEnvKey {
    driver::round_env(db, round)
}

/// One component's resolution at one round.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn scc_state(db: &Db, key: SccKey) -> SccState {
    frame::task(Tag::Nameres, |frame| unit::scc_state(db, frame, key))
}

/// The round whose state `scc_state(key)` hands out: `key`'s round, or the last before it at
/// which the component changed.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn latest(db: &Db, key: SccKey) -> u32 {
    scc_state(db, key).round
}

/// The module's `ModuleScope` after the round's import fixed point. Backdates while the
/// component hands out the same one.
#[blaze_query::tracked(returns(copy))]
pub(crate) fn fact_handle(db: &Db, key: FactKey) -> blaze_query::db::Id {
    unit::fact_handle(db, key)
}

/// The module's plain imports still unresolved after the round's fixed point, by the name each
/// binds, in import order: what case 1 of a step's macro calls reads (`unresolved_named`).
#[blaze_query::tracked(returns(ref))]
pub(crate) fn unresolved_named(db: &Db, key: FactKey) -> Box<[(Name, ModPath)]> {
    unit::unresolved_named(db, key)
}

/// One module's macro resolution at one round.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn macro_step(db: &Db, key: FactKey) -> MacroStep {
    frame::task(Tag::Nameres, |frame| macros::macro_step(db, frame, key))
}

#[blaze_query::tracked(returns(copy))]
pub(crate) fn round_progress(db: &Db, round: RoundKey) -> RoundOutcome {
    driver::round_progress(db, round)
}

#[blaze_query::tracked(returns(copy))]
pub(crate) fn round_kind(db: &Db, round: RoundKey) -> RoundKind {
    driver::round_kind(db, round)
}

/// The row of `module` in a view that does not hold it.
#[blaze_query::tracked(returns(ref))]
pub(crate) fn row(db: &Db, rows: RowsKey, module: ModuleId) -> Option<ModuleData> {
    match rows.rows(db) {
        Rows::Round(round) => driver::round_row(db, round, module),
        Rows::Decl(key) => collect::decl_row(db, key, module),
    }
}

/// The `ModuleScope` (as `blaze_query::db::Id`) a `ScopeSlot::Fact` names.
pub(crate) fn scope_of(db: &Db, scope: ScopeRef) -> blaze_query::db::Id {
    match scope.scope(db) {
        Scope::Fact(key) => fact_handle(db, key),
        Scope::Decl(key) => decl_handle(db, key),
    }
}

/// A `ScopeSlot::Fact` for `scope`.
pub(super) fn fact_slot(db: &Db, scope: Scope) -> crate::nameres::ScopeSlot {
    crate::nameres::ScopeSlot::Fact(blaze_query::db::AsId::as_id(&ScopeRef::new(db, scope)))
}
