//! The calls of a stateful expander, chained: the state each starts from is the state the call
//! before it left, in its calling crate's source order (target decision 7's "state is a read").
//!
//! A module whose crates write no static is always [`StartState::Fresh`] and needs no chain: its
//! calls commute, and each record stands alone. One that writes a static is run per calling
//! crate as a [`Chain`]: every call may start at once, from the state its predecessor is
//! predicted to leave (the last state known before it), so the calls of one crate still expand
//! in parallel; when a predecessor ends with another state than the one its successor ran
//! with, the successor runs again from the confirmed state. A call runs at most twice: its
//! second run starts from a confirmed state, which nothing changes after (bounded progress,
//! rule 18). A call's record is keyed by the state it ran with (the core's `CallContext::state`),
//! so a hit is found with the same chaining, and an edit that changes one call's
//! state misses that call and the calls after it, never one before.
//!
//! A chain is per request, single-owner (the driver's), and its decisions depend only on the
//! order of calls and the states reported, never on when the reports arrive.
//!
//! A call has at most one run in flight: a call found running from a stale prediction is not
//! given a second run beside it; its run's [`Chain::end`] names it to run again. Two runs of
//! one call in flight would let the stale one end after the confirmed one, an end of a call
//! that is no longer running (the regression `tests::chain` keeps).

use std::{alloc::Global, fmt};

use blaze_keyed::kind::StartState;

use crate::{Subject, macro_call::StateOut};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    /// Not started.
    Idle,
    /// Started with this state, not ended.
    Running { with: StartState, run: u8 },
    /// Ended: ran with `with`, left `left`; `confirmed` when `with` is what its predecessor,
    /// itself confirmed, left (or the chain's start, for the first call).
    Ended { with: StartState, left: StartState, run: u8, confirmed: bool },
}

/// The calls of one expander module from one calling crate, in source order.
pub struct Chain {
    subject: Subject,
    start: StartState,
    slots: Vec<Slot, Global>,
}

/// A misuse of a chain by its driver, naming the expander and the call (rule 19).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Misstep {
    /// A call past the chain's end.
    OutOfRange { subject: Subject, at: usize, len: usize },
    /// A call started again while running, or ended without starting.
    NotRunning { subject: Subject, at: usize },
    /// A call asked to run a third time: impossible when the driver reports what it ran.
    ThirdRun { subject: Subject, at: usize },
    /// A call ended with another state than the one its run in flight was given.
    OtherState { subject: Subject, at: usize },
}

impl fmt::Display for Misstep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Misstep::OutOfRange { subject, at, len } => {
                write!(f, "{subject}: call {at} of a chain of {len} calls (rule bounded-progress)")
            }
            Misstep::NotRunning { subject, at } => write!(
                f,
                "{subject}: call {at} was started while running or ended while not running \
                 (rule bounded-progress)"
            ),
            Misstep::ThirdRun { subject, at } => write!(
                f,
                "{subject}: call {at} would run a third time, though its second run starts from a \
                 confirmed state (rule bounded-progress)"
            ),
            Misstep::OtherState { subject, at } => write!(
                f,
                "{subject}: call {at} ended with another state than its run was given \
                 (rule bounded-progress)"
            ),
        }
    }
}

/// What ending a call decided: the calls to run again, each with the confirmed state it runs
/// from, in order. Each named call has no run in flight and is already marked running with that
/// state: the driver runs it from that state and [`Chain::end`]s it with it.
pub type Reruns = Vec<(usize, StartState), Global>;

fn left(with: StartState, out: StateOut) -> StartState {
    match out {
        StateOut::Unchanged => with,
        StateOut::Moved(state) => state,
    }
}

impl Chain {
    /// A chain of `len` calls, the first of which starts from `start` (the module's fresh
    /// state, or the state a calling crate's first call is given).
    pub fn new(subject: Subject, start: StartState, len: usize) -> Chain {
        let mut slots = Vec::with_capacity_in(len, Global);
        slots.resize(len, Slot::Idle);
        Chain { subject, start, slots }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    fn range(&self, at: usize) -> Result<(), Misstep> {
        match at < self.slots.len() {
            true => Ok(()),
            false => Err(Misstep::OutOfRange { subject: self.subject.clone(), at, len: self.slots.len() }),
        }
    }

    /// The state call `at` is confirmed to start from, when it is: the chain's start for the
    /// first call, else what a confirmed predecessor left.
    fn confirmed_input(&self, at: usize) -> Option<StartState> {
        if at == 0 {
            return Some(self.start);
        }
        match self.slots[at - 1] {
            Slot::Ended { left, confirmed: true, .. } => Some(left),
            _ => None,
        }
    }

    /// The state to start call `at` from: the confirmed one, else the last state known before
    /// it (a prediction), else the chain's start.
    fn predicted_input(&self, at: usize) -> StartState {
        if let Some(state) = self.confirmed_input(at) {
            return state;
        }
        for slot in self.slots[..at].iter().rev() {
            match *slot {
                Slot::Ended { left, .. } => return left,
                Slot::Running { with, .. } => return with,
                Slot::Idle => {}
            }
        }
        self.start
    }

    /// Starts call `at`: the state it runs (and is keyed) with.
    pub fn start(&mut self, at: usize) -> Result<StartState, Misstep> {
        self.range(at)?;
        let run = match self.slots[at] {
            Slot::Idle => 1,
            _ => return Err(Misstep::NotRunning { subject: self.subject.clone(), at }),
        };
        let with = self.predicted_input(at);
        self.slots[at] = Slot::Running { with, run };
        Ok(with)
    }

    /// Ends call `at`, which ran with `ran_with` (what [`Chain::start`] or a rerun gave it) and
    /// left `out`: the calls to run again, this one included when it ran from a stale
    /// prediction.
    pub fn end(&mut self, at: usize, ran_with: StartState, out: StateOut) -> Result<Reruns, Misstep> {
        self.range(at)?;
        let Slot::Running { with, run } = self.slots[at] else {
            return Err(Misstep::NotRunning { subject: self.subject.clone(), at });
        };
        if ran_with != with {
            return Err(Misstep::OtherState { subject: self.subject.clone(), at });
        }
        let confirmed = self.confirmed_input(at) == Some(with);
        self.slots[at] = Slot::Ended { with, left: left(with, out), run, confirmed };
        let mut reruns = Vec::new_in(Global);
        if confirmed {
            self.confirm_from(at + 1, &mut reruns)?;
        } else if let Some(input) = self.confirmed_input(at) {
            // Its predecessor is confirmed and left another state: run it again from that.
            self.rerun(at, input, &mut reruns)?;
        }
        Ok(reruns)
    }

    fn rerun(&mut self, at: usize, input: StartState, reruns: &mut Reruns) -> Result<(), Misstep> {
        let run = match self.slots[at] {
            Slot::Ended { run, .. } | Slot::Running { run, .. } => run,
            Slot::Idle => 0,
        };
        if run >= 2 {
            return Err(Misstep::ThirdRun { subject: self.subject.clone(), at });
        }
        self.slots[at] = Slot::Running { with: input, run: run + 1 };
        reruns.push((at, input));
        Ok(())
    }

    /// Confirms the ended calls from `at` on whose input is now confirmed, and names the first
    /// that ran with another state to run again.
    fn confirm_from(&mut self, mut at: usize, reruns: &mut Reruns) -> Result<(), Misstep> {
        while at < self.slots.len() {
            let input = self.confirmed_input(at).expect("its predecessor was just confirmed");
            match self.slots[at] {
                Slot::Ended { with, left, run, confirmed: false } if with == input => {
                    self.slots[at] = Slot::Ended { with, left, run, confirmed: true };
                    at += 1;
                }
                Slot::Ended { confirmed: false, .. } => return self.rerun(at, input, reruns),
                // Running (from the confirmed state, or a stale one): its end decides, since a
                // call has one run in flight at most.
                _ => return Ok(()),
            }
        }
        Ok(())
    }

    /// Whether every call has ended with a confirmed state: the chain's results are final.
    pub fn settled(&self) -> bool {
        self.slots.iter().all(|it| matches!(it, Slot::Ended { confirmed: true, .. }))
    }

    /// The state call `at` ran with and left, when it ended confirmed.
    pub fn confirmed(&self, at: usize) -> Option<(StartState, StartState)> {
        match self.slots.get(at)? {
            Slot::Ended { with, left, confirmed: true, .. } => Some((*with, *left)),
            _ => None,
        }
    }

    /// How many times call `at` was started.
    pub fn runs(&self, at: usize) -> u8 {
        match self.slots.get(at) {
            Some(Slot::Running { run, .. } | Slot::Ended { run, .. }) => *run,
            _ => 0,
        }
    }
}
