//! Passes and the class rule.
//!
//! A pass is a declared set of lints computed together over one subject, because they share a
//! walk (every lint of `suspicious` in one walk of a body). Records are per (pass, subject): one
//! [`crate::kinds::BodyFindings`] per body and pass, not per lint, so a body with forty lints on
//! is forty findings in one record, not forty lookups. A pass runs when any of its lints is in
//! the view's universe ([`crate::view::View::universe`]).
//!
//! **The class rule.** Every pass is declared with its class and its expected cost per subject.
//!
//! - [`Class::Answer`]: computed before the reply. Every pass with a lint at `deny` or `forbid`
//!   anywhere in the view is this class whatever it declared ([`class_of`]): an error is never
//!   deferred, so a gate never passes because its check was not reached. A fold's rows pass is
//!   promoted with its fold.
//! - [`Class::Cheap`]: per-body advisory passes and the delta of every workspace fold: computed
//!   right after the answer, within [`Budgets::cheap_ns`].
//! - [`Class::Deferred`]: expensive analyses (value ranges, near-duplicate search, mutation
//!   testing, tier-up): computed after the cheap class drains, at utility QoS, on the command's
//!   snapshot, cancelled by a newer command where they read something that moved.
//!
//! A declared cost is checked twice: against its class's bound at declaration
//! ([`PassDecl::misdeclared`]), and against the measured mean once a pass has run
//! ([`Measured::slow`]). A pass slower than its class's bound is reported by name with both
//! numbers (rule 16), never quietly moved to another class.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use blaze_keyed::machine::{Machine, Qos};

use crate::finding::{LintId, name_hash};
use crate::view::{Level, View};
use crate::{Plant, planted};

/// A pass by its name's hash.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PassId(pub u64);

impl PassId {
    pub const fn of(name: &str) -> PassId {
        PassId(name_hash(b'p', name.as_bytes()))
    }
}

/// A pass and the version of its code: what its records' keys name. A change to what the pass
/// computes bumps the version (once blaze's code identity reaches lint code, this becomes the
/// pass function's `code_id`, rule 52).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PassRef {
    pub id: PassId,
    pub version: u16,
}

/// When a pass's findings are computed.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Class {
    Answer = 0,
    Cheap = 1,
    Deferred = 2,
}

/// What a pass is computed over, which decides its record kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Over {
    /// One body: [`crate::kinds::BodyFindings`].
    Body,
    /// One item's rows for a fold: [`crate::kinds::ItemRows`].
    Rows,
    /// A workspace fold over the rows of the pass `rows`: [`crate::kinds::FoldFindings`].
    Fold { rows: PassId },
    /// An analysis of one item that reads beyond it: [`crate::kinds::DeferredFindings`].
    Analysis,
}

/// A pass as declared next to its lints.
#[derive(Clone, Copy, Debug)]
pub struct PassDecl {
    /// [`PassId::of`] its name, written out so a table of passes is a `const`.
    pub id: PassId,
    pub name: &'static str,
    pub version: u16,
    pub lints: &'static [LintId],
    pub over: Over,
    pub class: Class,
    /// The expected cost per subject, in nanoseconds.
    pub expected_ns: u64,
}

impl PassDecl {
    pub fn id(&self) -> PassId {
        self.id
    }

    pub fn reference(&self) -> PassRef {
        PassRef { id: self.id, version: self.version }
    }

    /// The declaration's own inconsistency, when it has one: an id that is not its name's.
    pub fn misnamed(&self) -> bool {
        self.id != PassId::of(self.name)
    }

    /// The declaration's own contradiction, when it has one: an expected cost over its class's
    /// bound.
    pub fn misdeclared(&self) -> Option<Slow> {
        let bound = bound_ns(self.class)?;
        (self.expected_ns > bound).then_some(Slow { pass: self.id(), mean_ns: self.expected_ns, bound_ns: bound })
    }
}

/// The most a subject of each class may cost: an answer pass is per body on the edit path (a
/// tenth of a body check's mean); a cheap pass's subject must fit the cheap budget many times
/// over. Deferred work has no bound per subject, only the budget a wait gives it.
pub fn bound_ns(class: Class) -> Option<u64> {
    match class {
        Class::Answer => Some(100_000),
        Class::Cheap => Some(1_000_000),
        Class::Deferred => None,
    }
}

/// A pass whose cost per subject is over its class's bound: reported with both numbers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slow {
    pub pass: PassId,
    pub mean_ns: u64,
    pub bound_ns: u64,
}

/// The class a pass runs in under `view`: [`Class::Answer`] when any of its lints is at deny or
/// above anywhere, else its declared class.
pub fn class_of(pass: &PassDecl, view: &View) -> Class {
    let gate = pass.lints.iter().any(|&lint| view.highest(lint) >= Level::Deny);
    match gate && !planted(Plant::DefersGate) {
        true => Class::Answer,
        false => pass.class,
    }
}

/// A pass's measured time: the count and the sum, so the mean is exact.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Measured {
    pub runs: u64,
    pub total_ns: u64,
}

impl Measured {
    pub fn add(&mut self, ns: u64) {
        self.runs += 1;
        self.total_ns = self.total_ns.saturating_add(ns);
    }

    pub fn mean_ns(&self) -> Option<u64> {
        (self.runs > 0).then(|| self.total_ns / self.runs)
    }

    /// The report of a pass slower than its class allows.
    pub fn slow(&self, pass: PassId, class: Class) -> Option<Slow> {
        let (mean_ns, bound_ns) = (self.mean_ns()?, bound_ns(class)?);
        (mean_ns > bound_ns).then_some(Slow { pass, mean_ns, bound_ns })
    }
}

/// Workers and the time a class may take after the answer, from the machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Budgets {
    /// Answer work: every core, at interactive QoS.
    pub answer_workers: u32,
    /// Cheap work: the performance cores, at interactive QoS.
    pub cheap_workers: u32,
    /// How long after the answer the default wait ([`crate::wait::Wait::Cheap`]) holds the reply
    /// for cheap work.
    pub cheap_ns: u64,
    /// Deferred work: a quarter of the cores, at least one, at utility QoS, so the editor and a
    /// build the person starts keep the machine.
    pub deferred_workers: u32,
}

impl Budgets {
    pub fn of(machine: &Machine) -> Budgets {
        let cores = machine.cores.max(1);
        Budgets {
            answer_workers: cores,
            cheap_workers: machine.fast_cores.clamp(1, cores),
            cheap_ns: 500_000_000,
            deferred_workers: (cores / 4).max(1),
        }
    }

    pub fn workers(&self, class: Class) -> u32 {
        match class {
            Class::Answer => self.answer_workers,
            Class::Cheap => self.cheap_workers,
            Class::Deferred => self.deferred_workers,
        }
    }
}

/// The QoS a class runs at.
pub fn qos(class: Class) -> Qos {
    match class {
        Class::Answer | Class::Cheap => Qos::Interactive,
        Class::Deferred => Qos::Utility,
    }
}
