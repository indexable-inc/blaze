//! The measures D161 is judged by (step 4): one asker's [`Counts`], and a body task's
//! [`Census`], which both the restart path and the ask path fill so one test reads them side by
//! side. Targets: `restarted_goals` 0, `entered_again` 0, `made_again` 0; `deepest` reported.
//! A measurement only: no answer, key or record depends on it.

use crate::gate::After;
use crate::need::{FACTS, Fact};

/// What one asker did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Root asks, by fact.
    pub asked: [u64; FACTS],
    /// Needs met inside a maker's step, by fact.
    pub needed: [u64; FACTS],
    /// Facts this asker made, by fact.
    pub made: [u64; FACTS],
    pub failed: u64,
    /// Needs found published (read, not made).
    pub read: u64,
    /// Waits that parked.
    pub parked: u64,
    /// Maker steps taken.
    pub steps: u64,
    /// The deepest work stack.
    pub deepest: u32,
}

impl Counts {
    pub fn add(&mut self, o: &Counts) {
        for i in 0..FACTS {
            self.asked[i] += o.asked[i];
            self.needed[i] += o.needed[i];
            self.made[i] += o.made[i];
        }
        self.failed += o.failed;
        self.read += o.read;
        self.parked += o.parked;
        self.steps += o.steps;
        self.deepest = self.deepest.max(o.deepest);
    }
}

/// A body task's census: how it met its facts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Census {
    /// Facts met after the body was entered, by fact. Of an item record ([`Fact::gated`]) one
    /// is a gap of the gate; the rest are expected and go through the one ask.
    pub after_entry: [u64; FACTS],
    /// Of those, the gate's gaps: item facts its set does not name ([`After::Gap`]).
    pub gaps: [u64; FACTS],
    /// Of those, item facts the gate's set names but did not settle before entry
    /// ([`After::Named`]): what settling the set takes away.
    pub named: [u64; FACTS],
    /// Entries into a body's check (every one, the first included), and of those the entries
    /// again after a need (target 0: on the restart path one per round past the first).
    pub entered: u64,
    pub entered_again: u64,
    /// Solver goals solved again after a fill (target 0; blaze-typeck `Solve::refills`).
    pub restarted_goals: u64,
    /// Facts made a second time under one key in one task (target 0): a restart's redone work.
    pub made_again: u64,
    /// The deepest work stack.
    pub deepest: u32,
}

impl Census {
    /// A need of `fact` met after entry, classed by the gate ([`GateSet::after_entry`](crate::GateSet::after_entry)).
    pub fn met_after_entry(&mut self, fact: Fact, after: After) {
        self.after_entry[fact.index()] += 1;
        match after {
            After::Expected => {}
            After::Named => self.named[fact.index()] += 1,
            After::Gap => self.gaps[fact.index()] += 1,
        }
    }

    /// What was counted after `before` (`deepest` is this one's).
    pub fn since(&self, before: &Census) -> Census {
        Census {
            after_entry: core::array::from_fn(|i| self.after_entry[i] - before.after_entry[i]),
            gaps: core::array::from_fn(|i| self.gaps[i] - before.gaps[i]),
            named: core::array::from_fn(|i| self.named[i] - before.named[i]),
            entered: self.entered - before.entered,
            entered_again: self.entered_again - before.entered_again,
            restarted_goals: self.restarted_goals - before.restarted_goals,
            made_again: self.made_again - before.made_again,
            deepest: self.deepest,
        }
    }

    pub fn add(&mut self, o: &Census) {
        for i in 0..FACTS {
            self.after_entry[i] += o.after_entry[i];
            self.gaps[i] += o.gaps[i];
            self.named[i] += o.named[i];
        }
        self.entered += o.entered;
        self.entered_again += o.entered_again;
        self.restarted_goals += o.restarted_goals;
        self.made_again += o.made_again;
        self.deepest = self.deepest.max(o.deepest);
    }
}
