//! What a change reaches ([`Front::reach`](crate::serve::work::Front::reach)), building
//! nothing more than each view's proc macros and build scripts' runs, which the job builds and
//! applies to its front end first, as a check does (`Jobs::next`): each member crate's items'
//! hashes ([`blaze_items::crate_hashes`]) before and after,
//! the items whose hashes differ with the facts that changed (a function's signature, a type's
//! layout, a body's code), the functions that would compile again, and the tests that reach
//! code that changed, from what the last compile of each test binary recorded.
//!
//! Cost: one merge of each crate's sorted key columns (`changed_since`), so `O(items)` per
//! member crate with nothing allocated per unchanged item; a path is resolved only for an item
//! that changed; each test is matched by walking its binary's table
//! (`ExecTable::reaches`) against a hash set of the old code hashes of what changed, stopping
//! at the first hit: `O(1)` per node walked, walked only here and never at build.

#![forbid(unsafe_code)]

use blaze_api::{FactRead, Reach, ReachedItem};
use blaze_hash::HashSet;
use blaze_items::{CodeHash, Fact, HashColumns, ItemHashes, ItemKey, ItemKind, Resolved};
use blaze_lower::Db;
use std::alloc::Global;

use super::{Job, answer_macros_built};
use crate::serve::work::Failed;

/// The facts of an item that differ from `old` to `new`, each with what it says.
pub(crate) fn changed_facts(
    key: ItemKey,
    old: Option<&ItemHashes>,
    new: Option<&ItemHashes>,
) -> Vec<(Fact, String)> {
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

/// The execution table of the test binary unit `at` made, `done`, when it lists the tests its
/// build listed, in order. Otherwise an error naming the binary (its package and target) and
/// why it has none, as without it nothing says what its tests reach.
fn table_of<'a>(
    from: &Job,
    at: usize,
    done: &'a super::compile::Made,
) -> Result<&'a blaze_codegen::ExecTable, Failed> {
    // alloc: error path
    let binary = || match from.units().graph.units.get(at) {
        Some(unit) => match from.package(unit) {
            Ok(package) => format!(
                "test binary `{}` of package `{} {}`",
                unit.target.name, package.name, package.version
            ),
            Err(_) => format!("test binary `{}` of package `{}`", unit.target.name, unit.pkg_id),
        },
        None => format!("test binary of unit {at}"),
    };
    let why = match &done.table {
        None => format!(
            "its record has no execution table, which only its link makes: it lists {} tests but was not linked in this view",
            done.tests.len()
        ),
        Some(table) if table.tests.len() != done.tests.len() => {
            format!(
                "its execution table lists {} tests where its build lists {}",
                table.tests.len(),
                done.tests.len()
            )
        }
        Some(table) => match table
            .tests
            .iter()
            .zip(&done.tests)
            .position(|(kept, listed)| kept.0 != listed.name)
        {
            None => return Ok(table),
            Some(n) => format!(
                "its execution table's test {n} is `{}` where its build lists `{}`",
                table.tests[n].0, done.tests[n].name
            ),
        },
    };
    Err(Failed::Error(blaze_api::ApiError::new(
        format!("what the tests of the {} reach can't be told: {why}", binary()),
        Some("reach".to_owned()),
    )))
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
    let mut changed_code: HashSet<CodeHash, Global> = HashSet::new_in(Global);
    let mut seen: HashSet<ItemKey, Global> = HashSet::new_in(Global);
    for (at, unit) in to.units().graph.units.iter().enumerate() {
        let package = to.package(unit)?;
        if !package.is_member || unit.is_std {
            continue;
        }
        let Some(new_krate) = to.crates.krate(at) else { continue };
        // A member crate's unit, by what it is in every view: its package, target and mode.
        let old_krate = from
            .graph
            .find(
                &from.units().graph.units,
                &unit.pkg_id,
                &unit.target.name,
                &unit.target.kind,
                unit.mode,
            )
            .and_then(|it| from.crates.krate(it as usize));
        if to.cancelled() {
            return Err(Failed::Again);
        }
        // A crate whose proc macros are not built has items that no hash covers
        // (`Missed::MacroPending`), which a diff would read as unchanged or removed. The job
        // built and applied every view's proc macros before posting this; checked again here.
        answer_macros_built(after, new_krate, &unit.target.name, "reach")?;
        let new: &HashColumns = blaze_items::crate_hashes(after, new_krate);
        let empty = HashColumns::default();
        let old: &HashColumns = match old_krate {
            Some(krate) => {
                answer_macros_built(before, krate, &unit.target.name, "reach")?;
                blaze_items::crate_hashes(before, krate)
            }
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
            let name = path(after, key)
                .or_else(|| path(before, key))
                .unwrap_or_else(|| format!("{:?}", key.0));
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
                facts: facts
                    .into_iter()
                    .map(|(fact, what)| FactRead { key: fact.key(), what })
                    .collect(),
            });
        }
    }
    let mut tests = Vec::new();
    for (at, done) in from.made.iter().enumerate().filter_map(|(at, it)| Some((at, it.get()?))) {
        if done.tests.is_empty() {
            continue;
        }
        let _span =
            tracing::info_span!(target: "blaze", "test reach", tests = done.tests.len()).entered();
        // Each test's reach is walked from its binary's table, stopping at the first change. A
        // binary without a table that lists its tests can't say what they reach: that is an
        // error naming it, never every test counted reached (no-fallback).
        let table = table_of(from, at, done)?;
        // A changed span table reaches every test of its binary (a panic prints positions).
        let spans = done.spans.iter().any(|it| changed_code.contains(it));
        for (n, test) in done.tests.iter().enumerate() {
            if test.outside || spans || table.reaches(n, |it| changed_code.contains(it)) {
                // clone: the response owns its names
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
