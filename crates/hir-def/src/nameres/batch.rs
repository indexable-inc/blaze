//! The expansions of one round of name resolution as batches of the database's tasks
//! ([`Db::run_all`]), before the collection reads them one at a time in the round's order.
//!
//! An expansion is a query of its call alone, so which task runs it when changes no result, and
//! collecting in the round's order keeps the def map the same whatever the scheduling. A proc
//! macro whose calls are ordered ([`ProcMacroExpander::ordered`]: its crate keeps a session,
//! or whether it does is not known) sees its calls from this crate in the round's order, as
//! rustc makes them: each such crate's calls are one task that expands them in that order, beside
//! the other tasks. Crates never share a session, so two crates' calls never wait on each other.
//!
//! Within one crate's calls, those after the first run ahead of their turn in parallel
//! ([`ProcMacroExpander::speculate`]) from the state the session is in once the first ran (all
//! of them, when an earlier round's call already ran in the session), and each call's turn
//! takes what its run gave only if the session is still in that state then; otherwise it runs
//! at its turn. So what each call gives is what it gives in order, and a macro
//! whose calls keep the state the first left (proc-macro2's detection, a `Once`) expands them
//! all at once.
//!
//! [`ProcMacroExpander::ordered`]: hir_expand::proc_macro::ProcMacroExpander::ordered
//! [`ProcMacroExpander::speculate`]: hir_expand::proc_macro::ProcMacroExpander::speculate

use base_db::{Crate, Db};
use blaze_mem::Tag;
use hir_expand::MacroCallId;

use crate::file_item_tree;

/// Expands `calls`, a round's calls within its recursion limit in the round's order, for
/// `krate`'s def map: the results are left in the database for the collection to read.
pub(crate) fn expand_round(db: &Db, krate: Crate, calls: &[MacroCallId]) {
    if calls.len() < 2 {
        return;
    }
    // alloc: once per round, the calls not ordered
    let mut free = Vec::with_capacity(calls.len());
    // alloc: once per round, each ordered proc-macro crate's calls in the round's order
    let mut ordered: Vec<(Crate, Vec<MacroCallId>)> = Vec::new();
    for &call in calls {
        match call.proc_macro_ordered(db) {
            Some((library, true)) => match ordered.iter_mut().find(|it| it.0 == library) {
                Some((_, them)) => them.push(call),
                None => ordered.push((library, vec![call])), // alloc: as above
            },
            _ => free.push(call),
        }
    }
    // A crate whose session already ran a call in this revision (an earlier round's) can run
    // all of this round's ahead; the others' first call runs alone, and the rest after it.
    // alloc: once per round, a flag per crate
    let warm: Vec<bool> =
        ordered.iter().map(|(_, them)| them[0].proc_macro_speculates(db)).collect();
    let span = tracing::info_span!(
        target: "blaze",
        "expand round",
        calls = calls.len(),
        free = free.len(),
        ordered = calls.len() - free.len(),
        libraries = ordered.len(),
        ahead = tracing::field::Empty,
    );
    let _span = span.enter();
    // The calls not ordered and the first of each cold crate's: each its own task.
    // alloc: once per round, the first batch's calls
    let mut first: Vec<MacroCallId> = free;
    first.extend(
        ordered.iter().zip(&warm).filter(|(_, warm)| !**warm).map(|((_, them), _)| them[0]),
    );
    // Each crate's calls left after the first batch, in order.
    // alloc: once per round, a list per crate
    let rest: Vec<&[MacroCallId]> = ordered
        .iter()
        .zip(&warm)
        .map(|((_, them), &warm)| if warm { &them[..] } else { &them[1..] })
        .filter(|them| !them.is_empty())
        .collect();
    if first.len() > 1 || (!first.is_empty() && !rest.is_empty()) {
        db.run_all(Tag::Expand, first.len(), |db, at| {
            file_item_tree(db, first[at].into(), krate);
        });
    }
    // Those of each crate whose session can give its state to a run ahead of a call's turn.
    // alloc: once per round, the calls run ahead
    let mut ahead = Vec::new();
    for them in &rest {
        if them[0].proc_macro_speculates(db) {
            ahead.extend_from_slice(them);
        }
    }
    span.record("ahead", ahead.len());
    if ahead.len() > 1 {
        db.run_all(Tag::Expand, ahead.len(), |db, at| ahead[at].speculate_proc_macro(db));
    }
    // Then each crate's in order, one task per crate: a call whose session moved on from the
    // state its run ahead started in runs here, at its turn. One crate's alone are left to the
    // collection, which takes them in the same order.
    if rest.len() > 1 {
        db.run_all(Tag::Expand, rest.len(), |db, at| {
            for &call in rest[at] {
                file_item_tree(db, call.into(), krate);
            }
        });
    }
}
