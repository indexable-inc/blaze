//! One run of a region (design §6): the groups it holds rebuilt from their round-0 logs on one
//! `facts` [`Unit`] whose view reads every other module as the state has it, through
//! `collect_block`'s loop: imports to their fixed point, a macro round per group, eager calls
//! with errors, attributes reseeded, then the unresolved imports recorded. Every read is noted
//! with its reader (`reads`), which is what the read index is made of.
//!
//! Each group keeps its own side tables and pending directives, and the sink attributes what a
//! group's macro round makes to that group, so a later run of some groups replaces just their
//! share of the crate's records, diagnostics and crate-level ops.

use base_db::{Crate, Db};
use blaze_hash::HashMap;
use blaze_mem::Frame;
use hir_expand::name::Name;

use super::{
    reads::{self, Read},
    state::{CrateState, GroupOut, clone_op},
};
use crate::{
    ModuleId, UseId,
    nameres::{
        DefMap, DiagnosticsByModule, LocalDefMap, ModulesMap, ModulesSlot,
        facts::{
            FrameVec, collect,
            driver::{FIXED_POINT_LIMIT, copy_diagnostic, fixpoint_limit},
            env::{ChildMode, ChildRequest, CrateEnv, SideTables, Sink},
            macros,
            ops::{DeclOp, MacroOp},
            queries,
            unit::Unit,
        },
        uses,
    },
};

/// What a run made.
pub(super) struct RunOut {
    /// The rows of the region's modules, scopes being built (`ScopeSlot::Building`), in the
    /// order the run made them.
    pub(super) rows: ModulesMap,
    /// Each group's share, and its modules in order, the round-0 one first.
    pub(super) outs: HashMap<ModuleId, (GroupOut, Vec<ModuleId>)>,
    /// The group of each module of `rows`.
    pub(super) group_of: HashMap<ModuleId, ModuleId>,
    /// What the run read, with whose read it was.
    pub(super) reads: Vec<Read>,
    /// The crate env after the run.
    pub(super) env: CrateEnv,
    /// The std prelude after the run, and the group whose `#[prelude_import]` it is.
    pub(super) prelude: Option<(ModuleId, Option<UseId>)>,
    pub(super) prelude_holder: Option<ModuleId>,
    /// An expansion wrote the crate root's scope, which the region did not hold.
    pub(super) missed_root: bool,
}

/// Runs `region` (groups of `state`, in group order) over `outside`, every row of the crate
/// but the region's (a module the run has not made yet does not exist), which it gives back.
pub(super) fn run(
    db: &Db,
    frame: &Frame,
    krate: Crate,
    state: &CrateState,
    region: &[ModuleId],
    outside: ModulesMap,
) -> (RunOut, ModulesMap) {
    let root = state.root;
    let env = match region.contains(&root) {
        true => root_env(db, state.groups[&root].key, &state.groups[&root].log),
        // clone: the crate env, a handful of entries, once per run
        false => state.env.clone(),
    };
    let mut reads = Vec::new(); // alloc: the run's reads, at the size the run makes
    let (mut out, outside) = reads::capture(&mut reads, || run_in(db, frame, krate, state, region, env, outside));
    out.reads = reads;
    (out, outside)
}

/// The crate env the root's collection starts from (`root_decl`'s), after the crate-level ops
/// of its round-0 log: `facts::driver::round_env` of round 0.
fn root_env(db: &Db, key: queries::DeclKey, log: &[DeclOp]) -> CrateEnv {
    // clone: the crate env, a handful of entries, once per run that holds the root
    let mut env = key.decl(db).env.env(db).krate.env(db).clone();
    for op in log {
        if let Some(next) = env.after(op) {
            env = next;
        }
    }
    env
}

fn run_in<'db>(
    db: &'db Db,
    frame: &Frame,
    krate: Crate,
    state: &CrateState,
    region: &[ModuleId],
    env: CrateEnv,
    outside: ModulesMap,
) -> (RunOut, ModulesMap) {
    let root = state.root;
    let cx = collect::cx(db, frame, krate, false);
    let def_map = DefMap {
        krate,
        block: None,
        root,
        modules: ModulesSlot::Maintained { own: ModulesMap::new(), outside },
        prelude: None,
        macro_use_prelude: HashMap::default(),
        derive_helpers_in_scope: HashMap::default(),
        session_macros: false,
        names: uses::NameRecords::default(),
        // alloc: empty, a run's view keeps no diagnostics
        diagnostics: Vec::new(),
        diagnostics_by_module: DiagnosticsByModule::default(),
        records: None,
        // clone: one count of the crate's shared data
        data: queries::crate_data(db, krate).clone(),
    };
    let mut unit = Unit::region(&cx, def_map, &env);
    if let Some(holder) = state.prelude_holder
        && !region.contains(&holder)
    {
        unit.def_map_mut().prelude = state.prelude;
    }
    let mut outs: HashMap<ModuleId, GroupOut> = HashMap::default();
    outs.reserve(region.len());
    let mut sink = RegionSink { unit, env, root, group: root, outs, missed_root: false };

    // Round 0: each group's log in group order, as the facts schedule replays a component's.
    let mut sides: FrameVec<'_, SideTables> = Vec::with_capacity_in(region.len(), frame);
    let mut pending: FrameVec<'_, FrameVec<'_, MacroOp>> = Vec::with_capacity_in(region.len(), frame);
    for &group in region {
        let _reading = reads::reading(group);
        let decls = queries::module_decls(db, state.groups[&group].key);
        let out = sink.outs.entry(group).or_default();
        let mut directives = Vec::new_in(frame);
        for op in decls.ops.iter() {
            match op {
                DeclOp::Splice { .. } => {
                    if group == root {
                        sink.unit.replay(op);
                    }
                }
                // clone: a directive the run owns until it resolves, once per directive
                DeclOp::Macro(it) if it.module == group => directives.push(it.clone()),
                DeclOp::Diagnostic(it) => out.diagnostics.push(copy_diagnostic(it)),
                DeclOp::NotIntroducing(_) | DeclOp::ResolvedMacroPath { .. } | DeclOp::DeriveHelperUse(_) => {
                    out.uses_include |= reads_a_file(op);
                    out.records.push(clone_op(op));
                }
                DeclOp::ExternPrelude { .. }
                | DeclOp::MacroUsePrelude { .. }
                | DeclOp::Prelude { .. }
                | DeclOp::ExportedDerive { .. }
                | DeclOp::ProcMacroFn { .. }
                | DeclOp::SessionMacros => out.crate_ops.push(clone_op(op)),
                &DeclOp::UnresolvedExternCrate { name } => out.unresolved_extern_crates.push(name),
                op if op.target() == Some(group) && op.is_resolution_op() => sink.unit.replay(op),
                _ => {}
            }
        }
        out.diagnostic_ends[0] = diagnostic_count(out);
        // clone: the module's tables after round 0, where its first macro round starts
        sides.push(SideTables::clone(&decls.side));
        pending.push(directives);
    }

    // The old `resolution_loop`, one macro round per group.
    let mut rounds = 0;
    let mut eager_with_errors = false;
    'resolve_attr: loop {
        'resolve_macros: loop {
            blaze_query::db::unwind_if_cancelled(db);
            sink.unit.set_unexpanded(pending.iter().any(|it| !it.is_empty()));
            sink.unit.resolve_imports();
            let (mut progress, mut deferred_eager) = (false, false);
            for (at, &group) in region.iter().enumerate() {
                sink.group = group;
                let result =
                    macros::resolve_round(&cx, &mut sink, &mut sides[at], &mut pending[at], eager_with_errors);
                progress |= result.progress;
                deferred_eager |= result.deferred_eager;
            }
            if !progress {
                if eager_with_errors || !deferred_eager {
                    break 'resolve_macros;
                }
                eager_with_errors = true;
                continue 'resolve_macros;
            }
            eager_with_errors = false;
            rounds += 1;
            if rounds > FIXED_POINT_LIMIT {
                let imports = sink.unit.import_outcomes().filter(|(_, it)| it.is_none()).map(|(it, _)| it);
                fixpoint_limit(db, krate, false, rounds, pending.iter().flatten(), imports).throw();
            }
        }
        let first = (0..region.len()).find_map(|at| Some((at, macros::first_attr(&pending[at])?)));
        let Some((at, index)) = first else { break 'resolve_attr };
        sink.group = region[at];
        let _reading = reads::reading(region[at]);
        macros::reseed_at(&cx, &mut sink, &mut sides[at], &mut pending[at], index);
        eager_with_errors = false;
    }

    // The old `collect`'s end and `finish`.
    sink.unit.record_unresolved();
    for (at, &group) in region.iter().enumerate() {
        sink.group = group;
        let out = sink.out();
        out.diagnostic_ends[1] = diagnostic_count(out);
        let _reading = reads::reading(group);
        macros::unresolved_macro_diagnostics(&cx, &mut sink, &pending[at]);
    }
    let RegionSink { unit, env, outs: mut group_outs, missed_root, .. } = sink;

    // Which group each module the run made is in: its own, or its parent's.
    let mut modules: HashMap<ModuleId, (GroupOut, Vec<ModuleId>)> = HashMap::default();
    modules.reserve(region.len());
    let mut group_of: HashMap<ModuleId, ModuleId> = HashMap::default();
    let def_map = unit.def_map();
    let ModulesSlot::Maintained { own, .. } = &def_map.modules else {
        unreachable!("nameres delta: a run's view is `ModulesSlot::Maintained`")
    };
    group_of.reserve(own.len());
    for (module, row) in own.iter() {
        let group = match group_outs.contains_key(&module) {
            true => module,
            false => {
                let parent = row.parent.expect("nameres delta: a module an expansion made has a parent");
                group_of[&parent]
            }
        };
        group_of.insert(module, group);
    }
    for (at, &group) in region.iter().enumerate() {
        let mut out = group_outs.remove(&group).unwrap_or_default();
        let side = &sides[at];
        stdx::always!(
            side.deferred_derives.is_empty(),
            "the deferred builtin derives of {group:?}: {:#?}",
            side.deferred_derives
        );
        for (&item, helpers) in &side.derive_helpers {
            // clone: each helper once, into the group's share of the crate's table
            out.derive_helpers.insert(item, helpers.to_vec());
        }
        // alloc: the group's modules, once per run
        modules.insert(group, (out, Vec::new()));
    }
    for (module, _) in own.iter() {
        modules.get_mut(&group_of[&module]).expect("a group of the region").1.push(module);
    }
    let mut prelude_holder = None;
    for (import, status) in unit.import_outcomes() {
        let group = group_of.get(&import.module).copied().unwrap_or_else(|| {
            panic!("nameres delta: an import of {:?}, which the run does not hold", import.module)
        });
        if import.source.is_prelude && status.is_some() {
            prelude_holder = Some(group);
        }
        // clone: the group's imports, which the crate's records and diagnostics are made from
        modules.get_mut(&group).expect("a group of the region").0.imports.push((import.clone(), status));
    }
    let prelude = unit.def_map().prelude();
    let def_map = unit.into_def_map();
    let ModulesSlot::Maintained { own: mut rows, outside } = def_map.modules else {
        unreachable!("nameres delta: a run's view is `ModulesSlot::Maintained`")
    };
    // The old `DefMap::shrink_to_fit` at the end of collection, which the published scopes
    // never reach.
    for (_, row) in rows.iter_mut() {
        row.children_mut().shrink_to_fit();
        row.scope_mut().settle_glob_ambiguities();
        row.scope_mut().settle();
    }
    let out = RunOut {
        rows,
        outs: modules,
        group_of,
        reads: Vec::new(), // alloc: empty, `run` puts the captured list here
        env,
        prelude,
        prelude_holder: prelude_holder.or(state.prelude_holder.filter(|it| !region.contains(it))),
        missed_root,
    };
    (out, outside)
}

/// Whether a record op is a call of `include!` (or a macro like it), whose expansion reads a
/// file no module is defined in.
fn reads_a_file(op: &DeclOp) -> bool {
    matches!(op, DeclOp::ResolvedMacroPath { def, .. } if def.is_include_like())
}

fn diagnostic_count(out: &GroupOut) -> u32 {
    u32::try_from(out.diagnostics.len()).expect("a group has 2^32 diagnostics")
}

/// The run's sink: the unit, with the crate-level ops, records and diagnostics a group's macro
/// round makes kept as that group's.
struct RegionSink<'db, 'f> {
    unit: Unit<'db, 'f, &'f Frame>,
    env: CrateEnv,
    root: ModuleId,
    /// The group whose macro round is running.
    group: ModuleId,
    outs: HashMap<ModuleId, GroupOut>,
    missed_root: bool,
}

impl RegionSink<'_, '_> {
    fn out(&mut self) -> &mut GroupOut {
        self.outs.entry(self.group).or_default()
    }

    fn holds(&self, module: ModuleId) -> bool {
        match &self.unit.def_map().modules {
            ModulesSlot::Maintained { own, .. } => own.contains_key(&module),
            _ => unreachable!("nameres delta: a run's view is `ModulesSlot::Maintained`"),
        }
    }
}

impl<'db> Sink<'db> for RegionSink<'db, '_> {
    fn db(&self) -> &'db Db {
        self.unit.db()
    }

    fn def_map(&self) -> &DefMap {
        self.unit.def_map()
    }

    fn local_def_map(&self) -> &LocalDefMap {
        self.unit.local_def_map()
    }

    fn apply(&mut self, op: DeclOp) {
        match op {
            DeclOp::ExternPrelude { .. } | DeclOp::MacroUsePrelude { .. } | DeclOp::Prelude { .. } => {
                if let Some(next) = self.env.after(&op) {
                    self.unit.set_env(&next);
                    self.env = next;
                }
                self.out().crate_ops.push(op);
            }
            DeclOp::ExportedDerive { .. } | DeclOp::ProcMacroFn { .. } | DeclOp::SessionMacros => {
                self.out().crate_ops.push(op);
            }
            DeclOp::UnresolvedExternCrate { name } => self.out().unresolved_extern_crates.push(name),
            DeclOp::Diagnostic(it) => self.out().diagnostics.push(it),
            op @ (DeclOp::NotIntroducing(_) | DeclOp::ResolvedMacroPath { .. } | DeclOp::DeriveHelperUse(_)) => {
                let out = self.out();
                out.uses_include |= reads_a_file(&op);
                out.records.push(op);
            }
            op => {
                if let Some(target) = op.target()
                    && !matches!(op, DeclOp::Init { .. })
                    && !self.holds(target)
                {
                    assert_eq!(
                        target, self.root,
                        "nameres delta: an expansion of {:?} writes {target:?}, which is not the crate root",
                        self.group
                    );
                    self.missed_root = true;
                    self.out().writes_root = true;
                    return;
                }
                if op.target() == Some(self.root)
                    && self.group != self.root
                    && matches!(op, DeclOp::Declare { .. } | DeclOp::Update { .. })
                {
                    self.out().writes_root = true;
                }
                self.unit.apply(op);
            }
        }
    }

    fn find_unresolved_named<T>(
        &self,
        module: ModuleId,
        name: Name,
        f: impl FnMut(&hir_expand::mod_path::ModPath) -> Option<T>,
    ) -> Option<T> {
        self.unit.find_unresolved_named(module, name, f)
    }

    fn child(&mut self, request: ChildRequest<'_>) -> ChildMode {
        self.unit.child(request)
    }

    fn take_macros(&mut self, into: &mut FrameVec<'_, MacroOp>) {
        self.unit.take_macros(into);
    }

    fn expanded_now(&mut self, module: ModuleId, call: hir_expand::MacroCallId) {
        self.unit.expanded_now(module, call);
    }

    fn collecting_expansion(&mut self, call: hir_expand::MacroCallId) {
        let db = self.unit.db();
        let file = hir_expand::HirFileId::from(call).original_file(db).file_id(db);
        let files = &mut self.out().expansion_files;
        let mut add = |file| {
            if !files.contains(&file) {
                files.push(file);
            }
        };
        add(file);
        collect::definition_files(db, call, add);
    }
}
