//! A write as a delta of round-0 logs (design §5): which groups' logs changed, which groups are
//! new or gone, and whether a change removed an op.
//!
//! A written file maps to the groups whose round-0 module it defines, and to their parents,
//! whose logs read the head of a child's file (`#![cfg]`, `#![macro_use]`). Each such group's
//! log is asked again under its stored key and compared with the stored copy. A changed list of
//! round-0 children, a changed crate root key (the dependencies' roots), an added or removed
//! file or a changed source root walks the whole round-0 tree instead, from `root_decl`: each
//! module's key comes from its parent's new log, and a module no longer declared is gone with
//! the group it heads.

use base_db::{Crate, Db, Written};
use blaze_mem::Tag;
use hir_expand::HirFileId;
use span::FileId;

use super::state::{CrateState, Group, GroupOut, LogDiff, RecordShare, clone_op, diff_log};
use crate::{
    FxIndexMap, ModuleId,
    nameres::facts::{
        ops::DeclOp,
        queries::{self, DeclKey},
    },
};

/// What a write changed of a crate's round-0 logs.
#[derive(Default)]
pub(super) struct Delta {
    /// The groups whose log changed or that are new, in group order.
    pub(super) seeds: Vec<ModuleId>,
    /// Of `seeds`, those whose change removed an op.
    pub(super) removing: Vec<ModuleId>,
    /// The groups whose change only inserted name-free ops (`state::is_name_free`), with
    /// their places in the new log: applied to the group's scopes without a run.
    pub(super) direct: Vec<(ModuleId, Vec<u32>)>,
    /// The groups no longer declared, as they were.
    pub(super) deleted: Vec<(ModuleId, Group)>,
    /// The groups whose log was read again because a file of theirs was written.
    pub(super) touched: Vec<ModuleId>,
    /// Whether the crate root's share of a log (`#[macro_export]`) changed.
    pub(super) root_ops: bool,
    /// Whether the round-0 tree changed: groups came or went.
    pub(super) tree: bool,
    pub(super) ops: u64,
}

/// Reads the crate's round-0 logs that `written` may have changed into `state`'s groups.
/// `cold`: the state is new, every group is a seed. `deps_changed`: a crate maintained before
/// this one changed a binding or a module in this write, so the root's log is read again: it
/// alone reads other crates at round 0 (the injected std prelude, `#[macro_use] extern crate`),
/// and `root_decl` holds only their roots. The root's log is read again too when the crate was
/// given proc macros (`written.proc_macros`): it lists their expanders, as `#[proc_macro]`
/// functions are only at the root.
pub(super) fn read(
    db: &Db,
    krate: Crate,
    state: &mut CrateState,
    written: &Written<'_>,
    cold: bool,
    deps_changed: bool,
) -> Delta {
    let mut delta = Delta::default();
    let deps_changed = deps_changed || written.proc_macros.binary_search(&krate).is_ok();
    let root_key = queries::root_decl(db, krate);
    let full = cold
        || !written.added.is_empty()
        || !written.removed.is_empty()
        || !written.roots.is_empty()
        || state.groups.get(&state.root).is_none_or(|it| it.key != root_key);
    if full || !incremental(db, state, written, deps_changed, &mut delta) {
        delta = Delta::default();
        walk(db, state, root_key, written, cold, &mut delta);
    }
    include_readers(state, written, &mut delta);
    expansion_readers(state, written, &mut delta);
    delta
}

/// Whether each parent from the root down to `group` still declares the key below it: a key may
/// name what the write took away (a `macro_rules!` its textual scope inherits), and its log,
/// read, would expand a call of it. The root's key is compared in [`read`].
fn key_is_current(db: &Db, state: &CrateState, group: ModuleId) -> bool {
    let mut chain = Vec::new(); // alloc: the group's ancestors, a few, once per checked group
    let mut at = group;
    while let Some(parent) = state.groups[&at].parent {
        let parent = state.group_of.get(&parent).copied().unwrap_or(parent);
        chain.push((parent, at));
        at = parent;
    }
    chain.iter().rev().all(|&(parent, child)| {
        queries::module_decls(db, state.groups[&parent].key).children.contains(&state.groups[&child].key)
    })
}

/// The groups of the written files (and the root when `deps_changed`, and those that expanded an
/// `include!` when a file no module is defined in was written), the parents of those whose logs
/// changed and the children defined in their expansions, their logs compared; `false` when a
/// group's round-0 children changed, which only a walk follows.
fn incremental(
    db: &Db,
    state: &mut CrateState,
    written: &Written<'_>,
    deps_changed: bool,
    delta: &mut Delta,
) -> bool {
    let mut checked: Vec<ModuleId> = Vec::new(); // alloc: the groups of the written files, once per write
    if deps_changed {
        checked.push(state.root);
    }
    // A written file no module is defined in may be one an `include!` read (`include_readers`).
    let unknown = |file: &FileId| !state.files.contains_key(file);
    if written.set.iter().any(unknown) || !written.included.is_empty() {
        checked.extend(state.groups.iter().filter(|(_, g)| g.out.uses_include).map(|(&group, _)| group));
    }
    for file in written.set {
        if let Some(groups) = state.other_files.get(file) {
            checked.extend_from_slice(groups);
        }
        let Some(groups) = state.files.get(file) else { continue };
        for &group in groups {
            delta.touched.push(group);
            checked.push(group);
            if let Some(parent) = state.groups[&group].parent {
                checked.push(state.group_of.get(&parent).copied().unwrap_or(parent));
            }
        }
    }
    // Every log is checked before any is compared: `compare` keeps the new log, so one kept
    // before a walk would hide its change from the walk. A parent's log reads its
    // `#[macro_use]` children's textual macros (`legacy_exports`), so a group whose log changed
    // has its parent checked too, up to the first parent whose log is as it was. A child defined
    // in a macro file has its log from its parent's expansion, which changes with no file of its
    // own written and its key as it was, so it is checked with its parent.
    let mut at = 0;
    while at < checked.len() {
        if !key_is_current(db, state, checked[at]) {
            return false;
        }
        let g = &state.groups[&checked[at]];
        at += 1;
        let decls = queries::module_decls(db, g.key);
        if decls.children != g.children || decls.other_files != g.other_files {
            return false;
        }
        for child in &decls.children {
            let decl = child.decl(db);
            if matches!(decl.tree.file_id(), HirFileId::MacroFile(_)) && !checked.contains(&decl.module) {
                checked.push(decl.module);
            }
        }
        if let Some(parent) = g.parent
            && decls.ops.ops != g.log
        {
            let parent = state.group_of.get(&parent).copied().unwrap_or(parent);
            if !checked.contains(&parent) {
                checked.push(parent);
            }
        }
    }
    checked.sort_by_key(|it| state.groups.get_index_of(it));
    checked.dedup();
    for group in checked {
        let g = &mut state.groups[&group];
        let decls = queries::module_decls(db, g.key);
        compare(state.root, group, g, &decls.ops.ops, delta);
    }
    true
}

/// The whole round-0 tree from `root_key`, in DFS preorder: the new group order.
fn walk(db: &Db, state: &mut CrateState, root_key: DeclKey, written: &Written<'_>, cold: bool, delta: &mut Delta) {
    delta.tree = true;
    decls_ahead(db, root_key);
    let mut old = std::mem::take(&mut state.groups);
    let mut groups = FxIndexMap::default();
    groups.reserve(old.len());
    state.files.clear();
    state.other_files.clear();
    let mut stack = vec![root_key]; // alloc: the walk's stack, once per walk
    while let Some(key) = stack.pop() {
        let decl = key.decl(db);
        let module = decl.module;
        let decls = queries::module_decls(db, key);
        let file = match decl.tree.file_id() {
            HirFileId::FileId(file) => Some(file.file_id(db)),
            HirFileId::MacroFile(_) => None,
        };
        let group = match old.swap_remove(&module) {
            Some(mut group) if !cold => {
                group.key = key;
                group.file = file;
                group.parent = decl.parent;
                // clone: the keys of the round-0 children, once per walk
                group.children = decls.children.clone();
                // clone: as above, empty for most modules
                group.other_files = decls.other_files.clone();
                compare(state.root, module, &mut group, &decls.ops.ops, delta);
                if file.is_some_and(|it| written.set.contains(&it)) {
                    delta.touched.push(module);
                }
                group
            }
            _ => {
                delta.seeds.push(module);
                delta.ops += decls.ops.ops.len() as u64;
                delta.root_ops |= module != state.root && decls.ops.writes_elsewhere();
                Group {
                    key,
                    // alloc: the state's copy of the log, once per log read
                    log: decls.ops.iter().map(clone_op).collect(),
                    file,
                    parent: decl.parent,
                    // clone: as above
                    children: decls.children.clone(),
                    // clone: as above
                    other_files: decls.other_files.clone(),
                    modules: Vec::new(), // alloc: empty, the group's run fills it
                    out: GroupOut::default(),
                    records: RecordShare::default(),
                }
            }
        };
        if let Some(file) = file {
            state.files.entry(file).or_default().push(module);
        }
        for &file in &decls.other_files {
            state.other_files.entry(file).or_default().push(module);
        }
        groups.insert(module, group);
        stack.extend(decls.children.iter().rev().copied());
    }
    delta.deleted.extend(old.drain(..));
    delta.root_ops |= delta.deleted.iter().any(|(_, it)| writes_root(state.root, &it.log));
    state.groups = groups;
}

/// Every round-0 log of the tree from `root_key`, a depth at a time, each a job of the pool:
/// a log parses its file's item tree, and the logs of one depth read nothing of each other, so
/// [`walk`] then reads each as a memo instead of parsing the crate's files one at a time.
/// Here and not in `module_decls`, which would read its children's logs and change with them.
fn decls_ahead(db: &Db, root_key: DeclKey) {
    let mut depth = vec![root_key]; // alloc: a depth of the round-0 tree, once per walk
    while !depth.is_empty() {
        let keys = &depth[..];
        db.run_all(Tag::Nameres, keys.len(), |db, at| _ = queries::module_decls(db, keys[at]));
        // alloc: the next depth, once per depth of a walk
        depth = keys.iter().flat_map(|&key| queries::module_decls(db, key).children.iter().copied()).collect();
    }
}

/// Compares the stored log of `group` with `new`, keeping a copy of `new` when they differ.
fn compare(root: ModuleId, module: ModuleId, group: &mut Group, new: &[DeclOp], delta: &mut Delta) {
    let LogDiff { changed, removed, ops, name_free } = diff_log(&group.log, new);
    if !changed {
        return;
    }
    delta.ops += ops;
    match name_free {
        Some(at) => delta.direct.push((module, at)),
        None => {
            delta.seeds.push(module);
            if removed {
                delta.removing.push(module);
            }
        }
    }
    if module != root && (writes_root(root, &group.log) || writes_root(root, new)) {
        let old = group.log.iter().filter(|op| is_root_op(root, op));
        delta.root_ops |= !old.eq(new.iter().filter(|op| is_root_op(root, op)));
    }
    // alloc: the state's copy of the log, once per changed log
    group.log = new.iter().map(clone_op).collect();
}

fn is_root_op(root: ModuleId, op: &DeclOp) -> bool {
    matches!(op, DeclOp::Declare { module, .. } | DeclOp::Update { module, .. } if *module == root)
}

fn writes_root(root: ModuleId, log: &[DeclOp]) -> bool {
    log.iter().any(|op| is_root_op(root, op))
}

/// An expansion's input is its call's syntax, which the round-0 log leaves out of an attributed
/// fn's body or a macro call's token tree: every group that expanded a call in a written file
/// is a seed, with a possible removal, so an expansion whose output changed is collected again.
/// A run that makes what was published sets nothing, so the crate's readers stay as they were.
fn expansion_readers(state: &CrateState, written: &Written<'_>, delta: &mut Delta) {
    for (&group, g) in &state.groups {
        if g.out.expansion_files.iter().any(|it| written.set.contains(it)) && !delta.seeds.contains(&group) {
            delta.seeds.push(group);
            delta.removing.push(group);
        }
    }
}

/// A written file that defines no module of the crate may be one an `include!` read, and an
/// `include_str!` or `include_bytes!` file whose bytes changed (`Written::included`) is one
/// such a call read: every group that expanded one is a seed, with a possible removal.
fn include_readers(state: &CrateState, written: &Written<'_>, delta: &mut Delta) {
    let unknown = |file: &FileId| !state.files.contains_key(file);
    if !written.set.iter().any(unknown) && written.included.is_empty() {
        return;
    }
    for (&group, g) in &state.groups {
        if g.out.uses_include && !delta.seeds.contains(&group) {
            delta.seeds.push(group);
            delta.removing.push(group);
        }
    }
}
