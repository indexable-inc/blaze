//! Provenance: what each memo brought up to a view did in it, and why it ran. A run records
//! one word on its node: the view and its cause, the first of its reads the claimed verify
//! found changed (an input field or a memo, by node), or why it ran with none. A hit and a
//! verify record nothing; [`Storage::provenance`] reads a view's table back from the nodes'
//! colors and these words.
//!
//! A cause is a node of the view's table when the memo was current in the view before (a
//! dependency that changed since is one that ran in this view); a memo verified in an earlier
//! view may name a dependency that ran in a view between, whose own word is that view's.

use std::sync::atomic::Ordering;

use super::{
    DatabaseKeyIndex, Read, Storage,
    capture::read_of_key,
    ingredient::{self, Kind},
    row::VIEW_MASK,
    storage::{Restamp, key_parts},
};

/// Why a memo ran.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cause {
    /// The first of its reads its verify found changed since it last ran: an input or
    /// tracked field or a memo, by node ([`Storage::node_read`]).
    Changed(u32),
    /// Its first run.
    New,
    /// Its value was evicted: proved unchanged, then run for a reader that wants the value.
    Evicted,
    /// It made an untracked read, so it runs in every view.
    Untracked,
    /// A cycle's: a stale provisional row, or a verify that re-entered a cycle.
    Cycle,
}

/// What a memo did in a view, besides a plain hit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Proved unchanged without running.
    Verified,
    /// Ran, and its value changed: its readers verify against it.
    Reran,
    /// Ran to a value equal to its old one: its readers stop here (early cutoff).
    ReranSameOutput,
}

/// One memo of a view's table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Provenance {
    /// Its node: what a [`Cause::Changed`] names.
    pub node: u32,
    pub key: DatabaseKeyIndex,
    /// Over the whole view: a memo first made in it is [`Outcome::Reran`] however it ran after.
    pub outcome: Outcome,
    /// Why it last ran in the view; `None` when verified.
    pub cause: Option<Cause>,
}

const REASON_SHIFT: u32 = 28;

impl Cause {
    /// The word a run in `view` records.
    pub(crate) fn word(self, view: u32) -> u64 {
        let (reason, node) = match self {
            Cause::Changed(node) => (0, node),
            Cause::New => (1, 0),
            Cause::Evicted => (2, 0),
            Cause::Untracked => (3, 0),
            Cause::Cycle => (4, 0),
        };
        (u64::from(view | reason << REASON_SHIFT) << 32) | u64::from(node)
    }

    /// The view and cause of a run's word; view 0 when the node never ran.
    fn of(word: u64) -> (u32, Cause) {
        let high = (word >> 32) as u32;
        let cause = match high >> REASON_SHIFT {
            0 => Cause::Changed(word as u32),
            1 => Cause::New,
            2 => Cause::Evicted,
            3 => Cause::Untracked,
            _ => Cause::Cycle,
        };
        (high & VIEW_MASK, cause)
    }
}

/// A run's word with its view passed through `restamp` (the renumbering).
pub(crate) fn restamp_run(word: u64, restamp: &mut Restamp<'_>) -> u64 {
    if word == 0 {
        return 0;
    }
    let (view, cause) = Cause::of(word);
    cause.word(restamp.apply(view))
}

impl Storage {
    /// The memos brought up to this view other than by a plain hit, pushed onto `out` in
    /// node order: each one's outcome and, if it ran, why. Read after the view's queries
    /// ended; a query still running is left out or shown as it was.
    pub fn provenance(&self, out: &mut Vec<Provenance>) {
        let view = self.view_bits();
        for node in 1..self.nodes_made() {
            let painted = self.color_of(node);
            if (painted >> 32) as u32 != view {
                continue;
            }
            let (index, id, _) = key_parts(self.key_of(node));
            if ingredient::kind(index) != Some(Kind::Function) {
                continue;
            }
            let (ran, cause) = Cause::of(self.runs.get(node as usize).load(Ordering::Relaxed));
            let outcome = match (ran == view, painted as u32 == view) {
                (false, _) => Outcome::Verified,
                (true, true) => Outcome::Reran,
                (true, false) => Outcome::ReranSameOutput,
            };
            // alloc: the caller's table, one entry per memo brought up to the view
            out.push(Provenance {
                node,
                key: DatabaseKeyIndex::new(index, id),
                outcome,
                cause: (ran == view).then_some(cause),
            });
        }
    }

    /// What node `node` is: a memo, or an input's or tracked struct's field.
    ///
    /// # Panics
    ///
    /// If no such node was made.
    pub fn node_read(&self, node: u32) -> Read {
        assert!(
            node != 0 && node < self.nodes_made(),
            "no node {node}: {} were made",
            self.nodes_made()
        );
        read_of_key(self.key_of(node))
    }
}
