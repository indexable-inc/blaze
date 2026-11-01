//! The maintainer's state per crate (design §6), outside the query database: what the last run of each group
//! made, the view of every module the next run reads, and who read what.
//!
//! A *group* is a round-0 module and the modules its macro expansions declare, which are all
//! inline below it. It is the unit a run rebuilds: its round-0 log replayed, its directives
//! expanded again (the expansions are memoized queries), its imports resolved again.

use base_db::Crate;
use blaze_dbsp::{Closure, Pos, ReadIndex};
use blaze_hash::{HashMap, HashSet};
use hir_expand::name::Name;
use span::FileId;
use syntax::ast;
use triomphe::Arc;

use super::publish::{CrateResolution, ScopeInput};
use crate::{
    AstId, BuiltinDeriveImplId, EnumId, FxIndexMap, ImplId, ModuleId, UseId,
    nameres::{
        DefMapCrateData, ModulesMap,
        diagnostics::DefDiagnostic,
        facts::{
            driver::copy_diagnostic,
            env::CrateEnv,
            ops::{DeclOp, DeriveHelper, ImportOp},
            queries::DeclKey,
        },
        uses::NameRecords,
    },
    per_ns::PerNs,
};

/// A read key: a name of a module, or the module whole.
pub(super) type Key = (ModuleId, Option<Name>);

/// Every crate's state, by crate. A crate served by the frozen image has none.
#[derive(Default)]
pub(in crate::nameres) struct State {
    pub(super) crates: HashMap<Crate, CrateState>,
    pub(super) pass: Pass,
}

/// One write's run across its steps, and buffers kept from one write to the next.
#[derive(Default)]
pub(super) struct Pass {
    /// The crates by layer of the crate graph: a step's jobs are a layer's crates.
    pub(super) layers: Vec<Vec<Crate>>,
    /// The modules whose row, children or textual macros changed, of the crates applied so
    /// far: `LastWrite` carries names only.
    pub(super) modules_changed: Vec<ModuleId>,
    /// A job's buffers each, taken by the job and given back by its apply.
    pub(super) scratch: Vec<Scratch>,
    /// The crates of the layer whose scopes the step published, for the next step's records.
    pub(super) scoped: Vec<super::Scoped>,
}

/// Buffers kept from one write to the next: a warm over-delete allocates nothing.
#[derive(Default)]
pub(super) struct Scratch {
    pub(super) closure: Closure<ModuleId>,
    pub(super) reached: Vec<ModuleId>,
}

pub(super) struct CrateState {
    pub(super) root: ModuleId,
    /// The groups by round-0 module, in round-0 DFS order (the old map's insertion order).
    pub(super) groups: FxIndexMap<ModuleId, Group>,
    /// Every module of the crate and the group it is in.
    pub(super) group_of: HashMap<ModuleId, ModuleId>,
    /// Every module as the next run reads it: a published scope (`ScopeSlot::Input`), or for a
    /// proc-macro crate's root the scope before the censor (`ScopeSlot::Building`), which the
    /// crate's own paths read. In no order: a run takes out and puts back its region's rows
    /// alone, and the published map is in group order ([`CrateState::ordered_rows`]).
    pub(super) rows: ModulesMap,
    pub(super) inputs: HashMap<ModuleId, ScopeInput>,
    /// Who read what: `(module, name)` (or the module whole) to the groups whose run read it,
    /// negative reads included; and each module to the groups that read anything of it.
    pub(super) reads: ReadIndex<Key, ModuleId>,
    pub(super) module_readers: ReadIndex<ModuleId, ModuleId>,
    /// The groups each file defines a round-0 module of.
    pub(super) files: HashMap<FileId, Vec<ModuleId>>,
    /// The groups whose round-0 log reads each file that defines none of their modules
    /// (`Group::other_files`).
    pub(super) other_files: HashMap<FileId, Vec<ModuleId>>,
    /// The crate env after the root's last run, which a run without the root starts from.
    pub(super) env: CrateEnv,
    /// The std prelude every path reads: the env's, or a `#[prelude_import]`'s, with the group
    /// whose import it is.
    pub(super) prelude: Option<(ModuleId, Option<UseId>)>,
    pub(super) prelude_holder: Option<ModuleId>,
    pub(super) resolution: Option<CrateResolution>,
    /// The position of each impl and builtin derive impl as `LastWrite` last handed it over:
    /// what a delete names.
    pub(super) impl_pos: HashMap<ImplId, Pos>,
    pub(super) derive_pos: HashMap<BuiltinDeriveImplId, Pos>,
    /// The position of each group (`diff::update_positions`), as of the last change of the
    /// round-0 tree.
    pub(super) group_pos: HashMap<ModuleId, Pos>,
    /// The crate data with every group's exported derives and proc macro functions, and whether
    /// a group asked for session macros: made again only when a group's crate-level ops change,
    /// or the root's crate attributes, [`Self::data_of`].
    pub(super) data: Option<(Arc<DefMapCrateData>, bool)>,
    /// The crate attributes `data` was made from (`queries::crate_data`): an edit of the root's
    /// `#![feature]` or `#![no_std]` changes them, and no group's ops.
    pub(super) data_of: Option<Arc<DefMapCrateData>>,
    /// A fingerprint of each globbed enum's variants (names, ids and shapes) as its glob
    /// readers last woke on: a write touching the enum wakes them only when it differs. An enum
    /// with no reader has none, and the first touch after a reader comes wakes it.
    pub(super) variants_seen: HashMap<EnumId, u64>,
}

impl CrateState {
    /// A state from empty that keeps the published inputs of `old`, which a cold run sets
    /// anew where they changed.
    pub(super) fn cold(root: ModuleId, old: Option<CrateState>) -> CrateState {
        let mut new = CrateState::new(root);
        if let Some(old) = old {
            new.inputs = old.inputs;
            new.resolution = old.resolution;
        }
        new
    }

    pub(super) fn new(root: ModuleId) -> CrateState {
        CrateState {
            root,
            groups: FxIndexMap::default(),
            group_of: HashMap::default(),
            rows: ModulesMap::new(),
            inputs: HashMap::default(),
            reads: ReadIndex::new(),
            module_readers: ReadIndex::new(),
            files: HashMap::default(),
            other_files: HashMap::default(),
            env: CrateEnv::default(),
            prelude: None,
            prelude_holder: None,
            resolution: None,
            impl_pos: HashMap::default(),
            derive_pos: HashMap::default(),
            group_pos: HashMap::default(),
            variants_seen: HashMap::default(),
            data: None,
            data_of: None,
        }
    }

    /// Every row in group then declaration order: the published map's order, the old map's
    /// insertion order.
    pub(super) fn ordered_rows(&self) -> ModulesMap {
        let mut rows = ModulesMap::new();
        rows.reserve(self.rows.len());
        for group in self.groups.values() {
            for &module in &group.modules {
                let row = self.rows.get(&module).unwrap_or_else(|| panic!("nameres delta: no row of {module:?}"));
                // clone: a published row (a scope input's id), once per change of the module tree
                rows.insert(module, row.clone());
            }
        }
        rows
    }

    /// The groups that read anything of a module of `group`, other than itself.
    pub(super) fn readers_of_group(&self, group: ModuleId, out: &mut Vec<ModuleId>) {
        let Some(g) = self.groups.get(&group) else { return };
        for &module in &g.modules {
            out.extend(self.module_readers.readers(module).filter(|&it| it != group));
        }
    }
}

/// One group: its round-0 module's key and log as last read, its modules, and what its last
/// run gave the crate.
pub(super) struct Group {
    pub(super) key: DeclKey,
    /// An owned copy of `module_decls(key).ops`, what the next write's log is compared with.
    pub(super) log: Box<[DeclOp]>,
    pub(super) file: Option<FileId>,
    pub(super) parent: Option<ModuleId>,
    /// `module_decls(key).children`: the keys of the round-0 modules it declares.
    pub(super) children: Box<[DeclKey]>,
    /// `module_decls(key).other_files`.
    pub(super) other_files: Box<[FileId]>,
    /// The group's modules in the order the last run made them, the round-0 one first.
    pub(super) modules: Vec<ModuleId>,
    pub(super) out: GroupOut,
    /// Its share of the crate's name records and import diagnostics, made after its last run.
    pub(super) records: RecordShare,
}

/// What a group's imports and record ops give the crate's [`OwnedRecords`]
/// (`crate::nameres::OwnedRecords`): `name_records` of its imports alone and of its record ops
/// alone, which the crate's records put every group's imports' before every group's record ops',
/// as the facts driver orders them, and its trait item imports' diagnostics.
#[derive(Default, PartialEq, Eq)]
pub(super) struct RecordShare {
    pub(super) imports: NameRecords,
    pub(super) macros: NameRecords,
    pub(super) trait_item_imports: Vec<DefDiagnostic>,
}

/// What a group's run made besides its modules' scopes, each list in the order it was made.
#[derive(Default, PartialEq, Eq)]
pub(super) struct GroupOut {
    /// Its imports and what each resolved to, `None` while unresolved.
    pub(super) imports: Vec<(ImportOp, Option<PerNs>)>,
    /// `NotIntroducing`, `ResolvedMacroPath` and `DeriveHelperUse` ops, its log's then its run's.
    pub(super) records: Vec<DeclOp>,
    /// Its log's diagnostics, then its run's, then its unresolved macros': the first
    /// `diagnostic_ends[0]` are its log's, up to `diagnostic_ends[1]` its run's. The crate's
    /// list is every group's log ones, then every group's run ones, then the rest, as the
    /// facts driver orders them.
    pub(super) diagnostics: Vec<DefDiagnostic>,
    pub(super) diagnostic_ends: [u32; 2],
    pub(super) derive_helpers: HashMap<AstId<ast::Item>, Vec<DeriveHelper>>,
    pub(super) unresolved_extern_crates: Vec<Name>,
    /// The crate-level ops it made (`ExternPrelude`, `MacroUsePrelude`, `Prelude`,
    /// `ExportedDerive`, `ProcMacroFn`, `SessionMacros`), its log's then its run's.
    pub(super) crate_ops: Vec<DeclOp>,
    /// Whether an expansion of it wrote the crate root's scope (`#[macro_export]` of a
    /// `macro_rules!` an expansion made): the root's run then needs this group's run too.
    pub(super) writes_root: bool,
    /// Whether it expanded an `include!`, which reads a file no module is defined in.
    pub(super) uses_include: bool,
    /// The files of the calls it expanded: an expansion's input is its call's syntax, fn bodies
    /// and token trees included, which a write may change with the group's log as it was.
    pub(super) expansion_files: Vec<FileId>,
}

/// A copy of `op`: logs are query values, and the state keeps the last one it read of each
/// group to compare the next with.
pub(super) fn clone_op(op: &DeclOp) -> DeclOp {
    if let Some(it) = op.clone_resolution_op() {
        return it;
    }
    match op {
        // clone: the state's copy of a round-0 log, once per log read
        DeclOp::Macro(it) => DeclOp::Macro(it.clone()),
        &DeclOp::Splice { child, key } => DeclOp::Splice { child, key },
        &DeclOp::SetModDir { module, dir } => DeclOp::SetModDir { module, dir },
        &DeclOp::Prelude { module, import } => DeclOp::Prelude { module, import },
        &DeclOp::ExternPrelude { name, root, extern_crate } => {
            DeclOp::ExternPrelude { name, root, extern_crate }
        }
        &DeclOp::UnresolvedExternCrate { name } => DeclOp::UnresolvedExternCrate { name },
        &DeclOp::MacroUsePrelude { name, mac, extern_crate } => {
            DeclOp::MacroUsePrelude { name, mac, extern_crate }
        }
        // clone: as above
        DeclOp::ExportedDerive { mac, helpers } => DeclOp::ExportedDerive { mac: *mac, helpers: helpers.clone() },
        &DeclOp::ProcMacroFn { func, mac } => DeclOp::ProcMacroFn { func, mac },
        &DeclOp::NotIntroducing(it) => DeclOp::NotIntroducing(it),
        DeclOp::Diagnostic(it) => DeclOp::Diagnostic(copy_diagnostic(it)),
        DeclOp::ResolvedMacroPath { module, path, bang, def, call } => DeclOp::ResolvedMacroPath {
            module: *module,
            // clone: as above
            path: path.clone(),
            bang: *bang,
            def: *def,
            call: *call,
        },
        &DeclOp::DeriveHelperUse(it) => DeclOp::DeriveHelperUse(it),
        DeclOp::SessionMacros => DeclOp::SessionMacros,
        _ => unreachable!("nameres delta: `clone_resolution_op` copies {op:?}"),
    }
}

/// Whether `old` and `new` differ, and whether an op of `old` is not in `new` (a removal, which
/// may take away what another group's binding rests on), with the ops that differ counted:
/// equal prefix and suffix skipped, the middle compared as multisets.
pub(super) fn diff_log(old: &[DeclOp], new: &[DeclOp]) -> LogDiff {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let (old, new) = (&old[prefix..], &new[prefix..]);
    let suffix = old.iter().rev().zip(new.iter().rev()).take_while(|(a, b)| a == b).count();
    let (old, new) = (&old[..old.len() - suffix], &new[..new.len() - suffix]);
    if old.is_empty() && new.is_empty() {
        return LogDiff { changed: false, removed: false, ops: 0, name_free: None };
    }
    // The middle is what an edit touched: a handful of ops, compared pairwise. Ops have no
    // hash, so a long middle (edits far apart) is counted as a removal without matching.
    if old.len() + new.len() > 256 {
        let ops = (old.len() + new.len()) as u64;
        return LogDiff { changed: true, removed: !old.is_empty(), ops, name_free: None };
    }
    let mut matched = vec![false; new.len()]; // alloc: the edited middle of one log, once per changed log
    let mut removed = false;
    for op in old {
        match (0..new.len()).find(|&at| !matched[at] && new[at] == *op) {
            Some(at) => matched[at] = true,
            None => removed = true,
        }
    }
    let added = matched.iter().filter(|it| !**it).count();
    let gone = old.len() - (new.len() - added);
    let inserted = || (0..new.len()).filter(|&at| !matched[at]);
    let name_free = (!removed && inserted().all(|at| is_name_free(&new[at])))
        // alloc: the inserted ops' places, a handful, once per changed log
        .then(|| inserted().map(|at| (prefix + at) as u32).collect());
    LogDiff { changed: true, removed, ops: (added + gone) as u64, name_free }
}

pub(super) struct LogDiff {
    pub(super) changed: bool,
    pub(super) removed: bool,
    pub(super) ops: u64,
    /// When the change only inserted ops that bind no name and that nothing in name resolution
    /// reads ([`is_name_free`]): their places in the new log.
    pub(super) name_free: Option<Vec<u32>>,
}

/// An op that adds to a module's scope a list entry no path reads: an impl, a builtin derive
/// impl, an unnamed `const`, an `extern` block.
pub(super) fn is_name_free(op: &DeclOp) -> bool {
    matches!(
        op,
        DeclOp::DefineImpl { .. }
            | DeclOp::DefineBuiltinDeriveImpl { .. }
            | DeclOp::DefineUnnamedConst { .. }
            | DeclOp::DefineExternBlock { .. }
    )
}

/// The groups of `seeds` and every group that read (transitively) anything of one of them:
/// DRed's over-delete region, closed under "read something in the region".
pub(super) fn over_delete(
    state: &CrateState,
    scratch: &mut Scratch,
    seeds: &[ModuleId],
    into: &mut HashSet<ModuleId>,
) {
    if seeds.is_empty() {
        return;
    }
    let Scratch { closure, reached } = scratch;
    closure.run(seeds.iter().copied(), |group, next| state.readers_of_group(group, next), reached);
    into.extend(reached.iter().copied());
}
