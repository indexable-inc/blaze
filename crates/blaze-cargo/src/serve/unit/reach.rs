//! What a change reaches ([`Front::reach`](crate::serve::work::Front::reach)), building
//! nothing: each member crate's items' hashes ([`blaze_items::crate_hashes`]) before and after,
//! the items whose hashes differ with the facts that changed (a function's signature, a type's
//! layout, a body's code), the functions that would compile again, and the tests that reach
//! code that changed, from what the last compile of each test binary recorded.
//!
//! Cost: one merge of each crate's sorted key columns (`changed_since`), so `O(items)` per
//! member crate with nothing allocated per unchanged item; a path is resolved only for an item
//! that changed; the tests are matched against a hash set of the old code hashes of what
//! changed, `O(1)` per hash a test reached.

#![forbid(unsafe_code)]

use blaze_api::{FactRead, Reach, ReachedItem};
use blaze_hash::HashSet;
use blaze_items::{CodeHash, Fact, HashColumns, ItemHashes, ItemKey, ItemKind, Resolved};
use blaze_lower::Db;

use super::Job;
use crate::serve::work::Failed;

/// The facts of an item that differ from `old` to `new`, each with what it says.
pub(crate) fn changed_facts(key: ItemKey, old: Option<&ItemHashes>, new: Option<&ItemHashes>) -> Vec<(Fact, String)> {
    let mut facts = Vec::new();
    let kind = new.or(old).map(|it| it.kind);
    let added = old.is_none();
    let removed = new.is_none();
    let what = |part: &str| match (added, removed) {
        (true, _) => format!("its {part}: added"),
        (_, true) => format!("its {part}: removed"),
        _ => format!("its {part} changed"),
    };
    if kind == Some(ItemKind::Fn) && old.map(|it| it.interface) != new.map(|it| it.interface) {
        facts.push((Fact::Signature(key), what("signature")));
    }
    if old.and_then(|it| it.layout) != new.and_then(|it| it.layout) {
        facts.push((Fact::Layout(key), what("layout")));
    }
    if old.and_then(|it| it.code) != new.and_then(|it| it.code) {
        facts.push((Fact::Code(key), what("code")));
    }
    facts
}

fn path(db: &Db, key: ItemKey) -> Option<String> {
    match blaze_items::resolve(db, key)? {
        Resolved::Item(id) => Some(blaze_items::key::path(db, id)),
        _ => None,
    }
}

/// What the change from `from`'s view, read through `before`, to `to`'s, read through `after`,
/// reaches.
pub(crate) fn reach(
    from: &Job,
    before: &Db,
    to: &Job,
    after: &Db,
    changes: &[blaze_vfs::Change],
) -> Result<Reach, Failed> {
    let mut items = Vec::new();
    let mut compiled = Vec::new();
    let mut changed_code: HashSet<CodeHash> = HashSet::default();
    let mut seen: HashSet<ItemKey> = HashSet::default();
    for (at, unit) in to.units().graph.units.iter().enumerate() {
        let package = to.package(unit)?;
        if !package.is_member || unit.is_std {
            continue;
        }
        let Some(new_krate) = to.crates.krate(at) else { continue };
        // A member crate's unit, by what it is in every view: its package, target and mode.
        let old_krate = from
            .graph
            .find(&from.units().graph.units, &unit.pkg_id, &unit.target.name, &unit.target.kind, unit.mode)
            .and_then(|it| from.crates.krate(it as usize));
        if to.cancelled() {
            return Err(Failed::Again);
        }
        let new: &HashColumns = blaze_items::crate_hashes(after, new_krate);
        let empty = HashColumns::default();
        let old: &HashColumns = match old_krate {
            Some(krate) => blaze_items::crate_hashes(before, krate),
            None => &empty,
        };
        for key in new.changed_since(old) {
            if !seen.insert(key) {
                continue;
            }
            let (was, now) = (old.get(key), new.get(key));
            let facts = changed_facts(key, was.as_ref(), now.as_ref());
            if facts.is_empty() {
                continue;
            }
            let name = path(after, key).or_else(|| path(before, key)).unwrap_or_else(|| format!("{:?}", key.0));
            if was.and_then(|it| it.code) != now.and_then(|it| it.code) {
                if let Some(code) = was.and_then(|it| it.code) {
                    changed_code.insert(code);
                }
                if now.is_some_and(|it| it.kind == ItemKind::Fn && it.code.is_some()) {
                    compiled.push(name.clone());
                }
            }
            items.push(ReachedItem {
                path: name,
                facts: facts.into_iter().map(|(fact, what)| FactRead { key: fact.key(), what }).collect(),
            });
        }
    }
    let mut tests = Vec::new();
    for done in from.made.iter().filter_map(|it| it.get()) {
        for test in &done.tests {
            if test.outside || test.reached.iter().any(|it| changed_code.contains(it)) {
                tests.push(test.name.clone());
            }
        }
    }
    compiled.sort_unstable();
    compiled.dedup();
    tests.sort_unstable();
    tests.dedup();
    Ok(Reach { changes: changes.to_vec(), items, compiled, tests })
}
