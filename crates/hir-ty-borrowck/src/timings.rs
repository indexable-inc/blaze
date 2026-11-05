//! Where the checker's time goes: [`measure`] runs a check with each phase of it timed and the
//! sizes it worked on counted, for `blaze-engine borrowck --project DIR --timings`. Outside of
//! [`measure`] a phase costs a thread-local read.

use std::{
    cell::RefCell,
    time::{Duration, Instant},
};

/// A phase of checking a body; each is timed across the body and the closures in it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// The interner, the param env and what `Ctx` gathers of the source.
    Setup,
    /// Lowering MIR: of the owner and the closures, coroutines and inline consts in it.
    Mir,
    /// The control flow graph, move paths, initialization and dominators.
    Moves,
    /// The outlives constraints of every statement, the universal regions and type tests.
    Constraints,
    /// Where each region is live.
    Liveness,
    /// Region inference, and member constraints of `impl Trait`.
    Infer,
    /// The loans and where they are activated.
    Loans,
    /// Where each loan is in scope: NLL's from region values, Polonius's loan liveness.
    Scopes,
    /// `polonius-flow`'s datalog loan liveness and universal check, inside [`Phase::Scopes`]:
    /// run only on a body the alpha rejects.
    Flow,
    /// The loans in scope at each block's entry.
    EntrySets,
    /// The walk over the body: conflicting accesses, moves and uninitialized uses.
    Walk,
    /// Region errors (what outlives what it may not), move errors and `unused_mut`.
    Errors,
}

impl Phase {
    pub const ALL: [Phase; 12] = [
        Phase::Setup,
        Phase::Mir,
        Phase::Moves,
        Phase::Constraints,
        Phase::Liveness,
        Phase::Infer,
        Phase::Loans,
        Phase::Scopes,
        Phase::Flow,
        Phase::EntrySets,
        Phase::Walk,
        Phase::Errors,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Phase::Setup => "setup",
            Phase::Mir => "mir",
            Phase::Moves => "moves",
            Phase::Constraints => "constraints",
            Phase::Liveness => "liveness",
            Phase::Infer => "infer",
            Phase::Loans => "loans",
            Phase::Scopes => "scopes",
            Phase::Flow => "flow",
            Phase::EntrySets => "entry_sets",
            Phase::Walk => "walk",
            Phase::Errors => "errors",
        }
    }
}

/// The sizes a check worked on, summed over the body and the closures in it.
#[derive(Clone, Copy, Default, Debug)]
pub struct Counts {
    pub bodies: usize,
    pub blocks: usize,
    pub points: usize,
    pub locals: usize,
    pub regions: usize,
    pub universals: usize,
    pub constraints: usize,
    /// Constraints that hold at one point only, Polonius's localized edges.
    pub localized: usize,
    pub loans: usize,
}

#[derive(Clone, Default, Debug)]
pub struct Timings {
    pub phases: [Duration; Phase::ALL.len()],
    pub counts: Counts,
}

impl Timings {
    pub fn get(&self, phase: Phase) -> Duration {
        self.phases[phase as usize]
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Timings>> = const { RefCell::new(None) };
}

/// Runs `f`, with the phases of the checks it runs on this thread timed.
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, Timings) {
    let outer = CURRENT.with(|it| it.borrow_mut().replace(Timings::default()));
    let result = f();
    let timings = CURRENT.with(|it| std::mem::replace(&mut *it.borrow_mut(), outer));
    (result, timings.unwrap_or_default())
}

/// Runs `f` as part of `phase`. A phase run inside another counts only toward the inner one.
pub(crate) fn phase<R>(phase: Phase, f: impl FnOnce() -> R) -> R {
    let started = start();
    let result = f();
    finish(phase, started);
    result
}

/// The start of a phase [`finish`] ends, for one that is not a closure.
pub(crate) struct Started(Option<(Instant, [Duration; Phase::ALL.len()])>);

pub(crate) fn start() -> Started {
    Started(CURRENT.with(|it| it.borrow().as_ref().map(|it| (Instant::now(), it.phases))))
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "finishing a phase ends its start: taking it means a start is finished once"
)]
pub(crate) fn finish(phase: Phase, started: Started) {
    let Started(Some((started, before))) = started else { return };
    let elapsed = started.elapsed();
    CURRENT.with(|it| {
        if let Some(timings) = it.borrow_mut().as_mut() {
            // Time the nested phases took is theirs, not this one's.
            let nested: Duration =
                timings.phases.iter().zip(&before).map(|(after, before)| *after - *before).sum();
            timings.phases[phase as usize] += elapsed.saturating_sub(nested);
        }
    });
}

/// Adds to the sizes being counted.
pub(crate) fn count(f: impl FnOnce(&mut Counts)) {
    CURRENT.with(|it| {
        if let Some(timings) = it.borrow_mut().as_mut() {
            f(&mut timings.counts);
        }
    });
}
