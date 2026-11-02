//! What a run changed (design §6, §9): each read key whose value differs from the state's
//! before the write, which is the cutoff (a reader outside the run is woken by a changed key
//! alone), and the impls, builtin derive impls and traits that came or went, which
//! `LastWrite` hands to the impls maintainer.

use base_db::Db;
use blaze_dbsp::{Pos, Weight};
use blaze_hash::{HashMap, HashSet};

use super::state::{CrateState, Key};
use crate::{
    BuiltinDeriveImplId, ImplId, ModuleDefId, ModuleId, TraitId,
    item_scope::ItemScope,
    nameres::{ModuleData, ModulesMap, facts::queries::DeclKey},
};

/// A key whose value changed, and whether it had one before (a change or a removal, which may
/// take away what a reader's binding rests on, against an addition).
pub(super) type Changed = (Key, bool);

/// The keys of `module` that differ between `old` and `new` (`None`: no such module).
pub(super) fn keys(db: &Db, module: ModuleId, old: Option<&ModuleData>, new: Option<&ModuleData>, out: &mut Vec<Changed>) {
    match (old, new) {
        (None, None) => {}
        (None, Some(_)) => out.push(((module, None), false)),
        (Some(old), None) => {
            out.push(((module, None), true));
            out.extend(old.scope(db).entries().map(|(&name, _)| ((module, Some(name)), true)));
        }
        (Some(old), Some(new)) => {
            let (os, ns) = (old.scope(db), new.scope(db));
            let whole = old.origin != new.origin
                || old.visibility != new.visibility
                || old.parent != new.parent
                || old.children(db) != new.children(db)
                || !os.legacy_macros().eq(ns.legacy_macros())
                || !unnamed(os).eq(unnamed(ns))
                || os.glob_ambiguities() != ns.glob_ambiguities();
            if whole {
                out.push(((module, None), true));
            }
            let start = out.len();
            for (&name, res) in os.entries() {
                if ns.get(&name) != res {
                    out.push(((module, Some(name)), true));
                }
            }
            // An old entry that bound nothing (an unresolved import) is one the loop above
            // already put in when it now binds something.
            for (&name, res) in ns.entries() {
                if os.get(&name).is_none()
                    && !res.is_none()
                    && !out[start..].iter().any(|((_, it), _)| *it == Some(name))
                {
                    out.push(((module, Some(name)), false));
                }
            }
        }
    }
}

fn unnamed(scope: &ItemScope) -> impl Iterator<Item = crate::per_ns::PerNs> + '_ {
    scope.resolutions().filter(|(name, _)| name.is_none()).map(|(_, res)| res)
}

/// The impls, builtin derive impls and declared traits of a set of modules, before or after a
/// write, impls with their positions.
#[derive(Default)]
pub(super) struct Items {
    pub(super) impls: Vec<(ImplId, Pos)>,
    pub(super) derive_impls: Vec<(BuiltinDeriveImplId, Pos)>,
    pub(super) traits: HashSet<TraitId>,
}

impl Items {
    /// Adds `scope`'s, the module at `pos`.
    pub(super) fn add(&mut self, scope: &ItemScope, pos: &Pos) {
        self.impls.extend((0u32..).zip(scope.impls()).map(|(at, it)| (it, pos.child(at))));
        self.derive_impls.extend((0u32..).zip(scope.builtin_derive_impls()).map(|(at, it)| (it, pos.child(at))));
        self.traits.extend(scope.declarations().filter_map(|it| match it {
            ModuleDefId::TraitId(it) => Some(it),
            _ => None,
        }));
    }
}

/// The weighted changes from `old` to `new`: an impl gone is a delete at the position it was
/// handed over at, an impl new or moved an insert at its new one.
pub(super) fn items(
    state: &mut CrateState,
    old: &Items,
    new: Items,
    impls: &mut Vec<(ImplId, Pos, Weight)>,
    derive_impls: &mut Vec<(BuiltinDeriveImplId, Pos, Weight)>,
    traits: &mut Vec<(TraitId, Weight)>,
) {
    weighted(&mut state.impl_pos, &old.impls, new.impls, impls);
    weighted(&mut state.derive_pos, &old.derive_impls, new.derive_impls, derive_impls);
    traits.extend(old.traits.difference(&new.traits).map(|&it| (it, -1)));
    traits.extend(new.traits.difference(&old.traits).map(|&it| (it, 1)));
}

fn weighted<T: Copy + Eq + std::hash::Hash>(
    held: &mut HashMap<T, Pos>,
    old: &[(T, Pos)],
    new: Vec<(T, Pos)>,
    out: &mut Vec<(T, Pos, Weight)>,
) {
    let now: HashSet<T> = new.iter().map(|&(it, _)| it).collect(); // alloc: the touched modules' impls, once per write that changed them
    for &(it, _) in old {
        if !now.contains(&it)
            && let Some(pos) = held.remove(&it)
        {
            out.push((it, pos, -1));
        }
    }
    for (it, pos) in new {
        if held.get(&it) != Some(&pos) {
            // clone: the position kept and the one handed over
            out.push((it, pos.clone(), 1));
            held.insert(it, pos);
        }
    }
}

/// Sets the position of each group after a change of the round-0 tree (design §9), and gives
/// the groups whose position is new or changed: the only ones whose impls move. The root's
/// position is the root, a round-0 child's its parent group's `.child(1 + its index among the
/// parent's children)`; a module of a group is at the group's `.child(0).child(its index in
/// the group)`, and an impl at its module's `.child(its index in the scope)`. Siblings keep
/// their order, so positions order impls as the module tree does. The walk that found the tree
/// change visited every group, which this follows at the same cost.
pub(super) fn update_positions(state: &mut CrateState) -> Vec<ModuleId> {
    let mut moved = Vec::new(); // alloc: the groups that moved, once per change of the tree
    // Each round-0 child's parent group and index, from the lists the walk just read: the walk
    // reached every group but the root from one of them. `group_of` is as of the last run, and
    // a write that moves a module into or out of round 0 (an attribute that may be a macro)
    // leaves it naming a group that no longer declares the child.
    let mut declared: HashMap<DeclKey, (ModuleId, u32)> = HashMap::default(); // alloc: the tree's children, once per change of the tree
    declared.reserve(state.groups.len());
    for (&group, g) in &state.groups {
        for (at, &key) in (0u32..).zip(&g.children) {
            declared.insert(key, (group, at));
        }
    }
    let mut now: HashMap<ModuleId, Pos> = HashMap::default();
    now.reserve(state.groups.len());
    // Group order is DFS preorder: a parent's position is known before its children's.
    for (&group, g) in &state.groups {
        let pos = match g.parent {
            None => Pos::root(),
            Some(_) => {
                let Some(&(parent, at)) = declared.get(&g.key) else {
                    panic!(
                        "nameres delta: the walk reached group {group:?} (key {:?}) from no group's round-0 children",
                        g.key
                    )
                };
                now[&parent].child(1 + at)
            }
        };
        if state.group_pos.get(&group) != Some(&pos) {
            moved.push(group);
        }
        now.insert(group, pos);
    }
    state.group_pos = now;
    moved
}

/// The items of `modules` (of one group, at `pos`) as `rows` has them.
pub(super) fn items_of(db: &Db, rows: &ModulesMap, modules: &[ModuleId], pos: &Pos, into: &mut Items) {
    let base = pos.child(0);
    for (at, &module) in (0u32..).zip(modules) {
        if let Some(row) = rows.get(&module) {
            into.add(row.scope(db), &base.child(at));
        }
    }
}
