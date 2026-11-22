//! THE GATE (D161 (a); typeck.md "the gate"): the facts a body's text names, settled before the
//! body is entered. Its caller computes the set from two records, never from the body's check:
//!
//! - the **names record**: each path of the body's text resolved to an item gives that item's
//!   [`Fact::Head`]; each trait it names or has in scope gives the trait's [`Fact::Members`] and
//!   its whole impl bucket ([`Fact::ImplsOf`] at [`Detail::WHOLE`](crate::Detail::WHOLE));
//! - the **outline**: the body's file's [`Fact::Outline`], and the lang items its syntax implies
//!   (`?`, `for`, `..`, literals, closures: [`Fact::Lang`]).
//!
//! The caller asks each through the one ask (parking is fine: the body is not entered yet) and
//! then takes [`GateSet::settle`]'s [`Settled`] proof, which entering the body takes (rule 74). A
//! fact met after entry is classed by [`GateSet::after_entry`].

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::need::Need;

/// A body's gate set: sorted, each need once.
pub struct GateSet<A: Allocator> {
    needs: Vec<Need, A>,
}

/// The proof a body's gate holds: every need of its set was published (made or failed: a
/// failed head is settled, as its error type).
pub struct Settled<'g> {
    needs: &'g [Need],
}

/// A need of the set not published when the gate was settled (rule 19: names it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unsettled {
    pub need: Need,
}

/// How a fact met after entry is classed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum After {
    /// Met late by design (a body record, an inherent lookup, an impl question at one shape).
    Expected,
    /// An item fact the gate's set names, met after entry all the same: the set was computed
    /// but not settled before entry (settling is the step after the set, D161 step 3).
    Named,
    /// An item fact the gate's set does not name: a gap of the gate's computation, by fact.
    Gap,
}

impl<A: Allocator> GateSet<A> {
    /// The set of `needs`, sorted and each once.
    pub fn of(needs: impl IntoIterator<Item = Need>, alloc: A) -> GateSet<A> {
        let mut v = Vec::new_in(alloc);
        for n in needs {
            v.push(n);
        }
        v.sort_unstable();
        v.dedup();
        GateSet { needs: v }
    }

    pub fn needs(&self) -> &[Need] {
        &self.needs
    }

    pub fn holds(&self, need: &Need) -> bool {
        self.needs.binary_search(need).is_ok()
    }

    /// The proof, when `published` says each need of the set was published.
    pub fn settle(&self, mut published: impl FnMut(&Need) -> bool) -> Result<Settled<'_>, Unsettled> {
        for n in self.needs.iter() {
            if !published(n) {
                return Err(Unsettled { need: *n });
            }
        }
        Ok(Settled { needs: &self.needs })
    }

    /// How `need`, met after entry, is classed.
    pub fn after_entry(&self, need: &Need) -> After {
        match (need.fact.gated(need.detail), self.holds(need)) {
            (false, _) => After::Expected,
            (true, true) => After::Named,
            (true, false) => After::Gap,
        }
    }
}

impl Settled<'_> {
    /// The needs settled.
    pub fn needs(&self) -> &[Need] {
        self.needs
    }
}
