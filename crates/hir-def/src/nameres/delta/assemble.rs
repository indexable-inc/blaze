//! A crate's [`CrateResolution`](super::publish::CrateResolution) from its groups' shares
//! (design §9), as the facts driver's `finish` makes a crate's def map, in parts that a write
//! makes again only where what they read changed: the modules in group order ([`modules`]), the
//! crate data with the exported derives and proc macro functions ([`data`]), each group's share
//! of the name records and import diagnostics ([`shares`], for the groups that ran), and the
//! crate's records from the shares ([`records`]: the diagnostics of the logs, then of the runs,
//! then of unresolved macros, then of unresolved imports, then of trait item imports; the name
//! records of the imports, then of the record ops; the derive helpers in scope).

use base_db::{Crate, Db};
use blaze_hash::{HashMap, HashSet};
use blaze_mem::{Frame, Tag};
use hir_expand::name::Name;
use triomphe::Arc;

use super::{
    reads::{self, Read},
    state::{CrateState, GroupOut, RecordShare},
};
use crate::{
    ExternCrateId, Lookup, MacroId, ModuleId,
    nameres::{
        DefMap, DefMapCrateData, DiagnosticsByModule, LocalDefMap, ModulesMap, ModulesSlot,
        OwnedRecords, ScopeSlot,
        diagnostics::DefDiagnostic,
        facts::{
            driver::{
                Records, copy_diagnostic, in_unresolved_extern_crate, name_records,
                trait_item_import_diagnostics,
            },
            ops::{DeclOp, ImportOp},
            queries, set_in,
        },
        uses::{NameRecords, Use},
    },
    per_ns::PerNs,
};

/// The crate's modules in group order, the published map. `root_input` is the published
/// (censored) root scope of a proc-macro crate, whose state row keeps the scope before the
/// censor.
pub(super) fn modules(state: &CrateState, root_input: Option<blaze_query::db::Id>) -> ModulesMap {
    let mut modules = state.ordered_rows();
    if let Some(input) = root_input {
        modules[state.root].scope = ScopeSlot::Input(input);
    }
    modules
}

/// The crate data with every group's exported derives and proc macro functions, and whether a
/// group asked for session macros.
pub(super) fn data(db: &Db, krate: Crate, state: &CrateState) -> (Arc<DefMapCrateData>, bool) {
    // clone: one count of the crate's shared data, made anew below only when an op adds to it
    let mut data = queries::crate_data(db, krate).clone();
    let mut session_macros = false;
    for group in state.groups.values() {
        for op in &group.out.crate_ops {
            match op {
                DeclOp::ExportedDerive { mac, helpers } => {
                    // clone: a derive's helper names, once per derive the crate exports
                    Arc::make_mut(&mut data).exported_derives.insert(*mac, helpers.clone());
                }
                &DeclOp::ProcMacroFn { func, mac } => {
                    let data = Arc::make_mut(&mut data);
                    data.fn_proc_macro_mapping.insert(func, mac);
                    data.fn_proc_macro_mapping_back.insert(mac, func);
                }
                DeclOp::SessionMacros => session_macros = true,
                _ => {}
            }
        }
    }
    if let Some(it) = Arc::get_mut(&mut data) {
        it.shrink_to_fit();
    }
    (data, session_macros)
}

/// A view of the crate as `state` has it, over `rows` (the state's rows, moved in; [`rows_of`]
/// gives them back).
pub(super) fn view(krate: Crate, state: &CrateState, rows: ModulesMap) -> DefMap {
    let (data, session_macros) = state.data.as_ref().expect("nameres delta: the crate data is made before a view");
    let mut view = DefMap {
        krate,
        block: None,
        root: state.root,
        modules: ModulesSlot::Maintained { own: ModulesMap::new(), outside: rows },
        prelude: None,
        macro_use_prelude: HashMap::default(),
        derive_helpers_in_scope: HashMap::default(),
        session_macros: *session_macros,
        names: NameRecords::default(),
        // alloc: empty, the records hold the crate's diagnostics
        diagnostics: Vec::new(),
        diagnostics_by_module: DiagnosticsByModule::default(),
        records: None,
        // clone: one count of the crate's shared data
        data: data.clone(),
    };
    state.env.install(&mut view);
    view.prelude = state.prelude;
    view
}

/// The crate's `macro_use` prelude, as its env installs it.
pub(super) fn macro_use_prelude(
    krate: Crate,
    state: &CrateState,
) -> HashMap<Name, (MacroId, Option<ExternCrateId>)> {
    let mut view = view(krate, state, ModulesMap::new());
    std::mem::take(&mut view.macro_use_prelude)
}

/// The rows a [`view`] was made over.
pub(super) fn rows_of(view: DefMap) -> ModulesMap {
    let ModulesSlot::Maintained { outside, .. } = view.modules else {
        unreachable!("nameres delta: the records' view is `ModulesSlot::Maintained`")
    };
    outside
}

/// The share of each group of `groups` on `view`, in that order, with what each read noted in
/// `reads` as that group's: a share is a pure function of the group's imports and record ops
/// and of what it reads, so a group that did not run and read nothing that changed keeps its
/// share.
pub(super) fn shares(
    db: &Db,
    state: &CrateState,
    view: &DefMap,
    local: &LocalDefMap,
    groups: &[ModuleId],
    reads: &mut Vec<Read>,
) -> Vec<RecordShare> {
    blaze_mem::frame::task(Tag::Nameres, |frame| {
        reads::capture(reads, || {
            groups
                .iter()
                .map(|&group| {
                    let _reading = reads::reading(group);
                    share(db, frame, &state.groups[&group].out, view, local)
                })
                // alloc: one share per group that ran, once per write
                .collect()
        })
    })
}

fn share(db: &Db, frame: &Frame, out: &GroupOut, view: &DefMap, local: &LocalDefMap) -> RecordShare {
    let imports = out.imports.iter().map(|(import, status)| (import, *status));
    let imports = name_records(db, frame, view, local, imports, Records::new_in(frame));
    let mut taken = Records::new_in(frame);
    for op in &out.records {
        taken.take(op);
    }
    let none = std::iter::empty::<(&ImportOp, Option<PerNs>)>();
    let macros = name_records(db, frame, view, local, none, taken);
    let mut trait_item_imports = Vec::new(); // alloc: the group's own list, mostly empty
    for (import, status) in &out.imports {
        trait_item_import_diagnostics(db, view, import, *status, |it| trait_item_imports.push(it));
    }
    RecordShare { imports, macros, trait_item_imports }
}

/// The crate's records from every group's output and share, in the facts driver's order.
pub(super) fn records(db: &Db, state: &CrateState) -> OwnedRecords {
    blaze_mem::frame::task(Tag::Nameres, |frame| records_in(db, frame, state))
}

fn records_in(db: &Db, frame: &Frame, state: &CrateState) -> OwnedRecords {
    let groups = || state.groups.values();
    let mut unresolved_extern_crates = set_in(frame);
    for group in groups() {
        unresolved_extern_crates.extend(group.out.unresolved_extern_crates.iter().copied());
    }

    // alloc: the crate's own list, once per write that changed it
    let mut diagnostics: Vec<DefDiagnostic> = Vec::new();
    for phase in 0..3 {
        for out in groups().map(|it| &it.out) {
            let [log, run] = out.diagnostic_ends.map(|it| it as usize);
            let range = match phase {
                0 => 0..log,
                1 => log..run,
                _ => run..out.diagnostics.len(),
            };
            diagnostics.extend(out.diagnostics[range].iter().map(copy_diagnostic));
        }
    }
    for (import, status) in groups().flat_map(|it| &it.out.imports) {
        if status.is_some() || in_unresolved_extern_crate(&import.path, &unresolved_extern_crates) {
            continue;
        }
        diagnostics.push(DefDiagnostic::unresolved_import(
            import.module,
            import.source.id.lookup(db).id,
            import.source.use_tree,
        ));
    }
    for group in groups() {
        diagnostics.extend(group.records.trait_item_imports.iter().map(copy_diagnostic));
    }

    let shares = || groups().map(|it| &it.records);
    let halves = || shares().map(|it| &it.imports).chain(shares().map(|it| &it.macros));
    let mut seen: HashSet<Use> = HashSet::default(); // alloc: once per write that changed the records
    let names = NameRecords {
        // alloc: the records' own lists, at exact size, once per write that changed them
        uses: halves().flat_map(|it| it.uses.iter().copied()).filter(|&it| seen.insert(it)).collect(),
        // alloc: as `uses`
        unresolved: halves().flat_map(|it| it.unresolved.iter().copied()).collect(),
        // alloc: as `uses`
        glob_targets: halves().flat_map(|it| it.glob_targets.iter().copied()).collect(),
        // alloc: as `uses`
        macro_calls: halves().flat_map(|it| it.macro_calls.iter().copied()).collect(),
        // alloc: as `uses`
        derive_helpers: halves().flat_map(|it| it.derive_helpers.iter().copied()).collect(),
        // alloc: as `uses`
        not_introducing: halves().flat_map(|it| it.not_introducing.iter().copied()).collect(),
    };

    let mut derive_helpers_in_scope = crate::nameres::DeriveHelpers::default();
    for out in groups().map(|it| &it.out) {
        // clone: each helper once, into the crate's table
        derive_helpers_in_scope.extend(out.derive_helpers.iter().map(|(&item, it)| (item, it.clone())));
    }
    diagnostics.shrink_to_fit();
    derive_helpers_in_scope.shrink_to_fit();
    OwnedRecords {
        names,
        diagnostics_by_module: DiagnosticsByModule::of(&diagnostics),
        diagnostics,
        derive_helpers_in_scope,
    }
}
