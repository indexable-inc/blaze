//! The session: a plan run within a hard deadline, as a sans-IO state machine.
//!
//! The session owns no IO and no clock. It hands out [`Effect`]s; a driver performs each and
//! hands back a [`Completion`] under the effect's [`Ticket`], with the time it observed. The
//! effects are what a mutant needs and nothing more:
//!
//! - [`Effect::Lookup`]: one batched read of every candidate's verdict record.
//! - [`Effect::Make`]: rewrite one function's checked MIR by the mutation and make its instance
//!   records, and those of its inliners, at the fast tier, into a scratch namespace that no
//!   shared cache reads or pushes.
//! - [`Effect::Patch`]: a scratch copy of one program's manifest with the mutant's instances
//!   placed (a delta, never a link).
//! - [`Effect::Run`]: one test of that patched program, in the sandbox, in its own process,
//!   killed at the timeout (a hang is a kill by timeout, never a wait).
//! - [`Effect::Put`]: file a final verdict.
//! - [`Effect::Drop`]: drop the mutant's scratch records and patched manifests.
//!
//! Every effect that takes time carries the session's deadline; the driver stops it there and
//! answers [`Completion::Expired`]. A test's timeout is cut to the time left; a test cut that way
//! and timed out is not a kill but [`NotRun::Expired`]. No more than [`Budget::slots`] mutants are
//! in flight; nothing is started at or after the deadline.
//!
//! [`Budget::slots`]: crate::select::Budget::slots

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use blaze_program::key::ItemId;
use blaze_program::record::TestKey;

use crate::select::{MutantRef, Plan, fit};
use crate::site::ProgramRef;
use crate::verdict::{DecodeError, KilledBy, NotRun, Verdict, VerdictKey, VerdictRecord};

/// Names one effect's completion.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Ticket(pub u32);

/// One mutant of the session: its index in [`Plan::candidates`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Mutant(pub u32);

/// What the driver is asked to do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Effect {
    Lookup { ticket: Ticket, keys: Vec<VerdictKey, Global> },
    Make { ticket: Ticket, mutant: Mutant, at: MutantRef, inliners: Vec<ItemId, Global>, deadline: u64 },
    Patch { ticket: Ticket, mutant: Mutant, program: ProgramRef, deadline: u64 },
    Run { ticket: Ticket, mutant: Mutant, test: TestKey, program: ProgramRef, timeout_ns: u64, deadline: u64 },
    /// No completion: the driver files it or fails the session loudly.
    Put { key: VerdictKey, bytes: Vec<u8, Global> },
    /// No completion.
    Drop { mutant: Mutant },
}

/// How a test ended.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TestOutcome {
    Passed,
    Failed,
    Crashed,
    TimedOut,
}

/// What the driver answers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Completion {
    /// For a lookup: each key's record bytes, in the keys' order.
    Found(Vec<Option<Vec<u8, Global>>, Global>),
    Made,
    Patched,
    Ran(TestOutcome),
    /// The deadline came first.
    Expired,
    /// The effect could not be done, and what it says why. A mutation compiles by construction,
    /// so a failed make is a bug of the front end's, shown, never skipped.
    Failed { what: Vec<u8, Global> },
}

/// The stage an effect failed at.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Stage {
    Make,
    Patch,
    Run,
}

/// Why one mutant has no verdict.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum MutantError {
    /// Its filed record is not a verdict record.
    Decode(DecodeError),
    Effect { stage: Stage, what: Vec<u8, Global> },
}

/// A driver's mistake: the session cannot go on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunError {
    UnknownTicket { ticket: Ticket },
    /// A completion of another effect's kind.
    WrongCompletion { ticket: Ticket, expected: &'static str },
    /// A lookup answered with a different number of values than keys.
    LookupCount { keys: u32, values: u32 },
    /// [`Session::finish`] before every effect completed.
    NotDone { pending: u32 },
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::UnknownTicket { ticket } => write!(f, "a completion for ticket {}, which no effect has", ticket.0),
            RunError::WrongCompletion { ticket, expected } => {
                write!(f, "ticket {} completed with the wrong kind: expected {expected}", ticket.0)
            }
            RunError::LookupCount { keys, values } => {
                write!(f, "a verdict lookup of {keys} keys answered with {values} values")
            }
            RunError::NotDone { pending } => write!(f, "the mutants session finished with {pending} effects pending"),
        }
    }
}

/// Where a verdict came from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Source {
    /// Filed before, under an equal key.
    Cached,
    Ran,
    /// Decided by the planner: not reached, equivalent.
    Static,
}

/// What the session did.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Stats {
    pub cached: u32,
    pub made: u32,
    pub patched: u32,
    pub runs: u32,
    pub put: u32,
    /// The most mutants in flight at once.
    pub peak: u32,
}

/// The session's result.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Outcome {
    /// Every mutant of the plan, candidates first in the plan's order, then the decided ones.
    pub verdicts: Vec<(MutantRef, Verdict, Source), Global>,
    pub errors: Vec<(MutantRef, MutantError), Global>,
    pub stats: Stats,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pending {
    Lookup,
    Make(u32),
    Patch(u32, ProgramRef),
    /// The mutant, the position in its tests, and whether the timeout was cut to the deadline.
    Run(u32, u32, bool),
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum Final {
    Verdict(Verdict, Source),
    Error(MutantError),
}

#[derive(Clone, Debug)]
struct State {
    end: Option<Final>,
    patched: BTreeSet<ProgramRef, Global>,
    /// The next position in the candidate's tests.
    next: u32,
    passed: Vec<TestKey, Global>,
}

/// One session.
#[derive(Debug)]
pub struct Session {
    plan: Plan,
    now: u64,
    deadline: u64,
    out: VecDeque<Effect, Global>,
    pending: BTreeMap<Ticket, Pending, Global>,
    next_ticket: u32,
    states: Vec<State, Global>,
    /// Candidates to start, in order, and how many are started.
    queue: Vec<u32, Global>,
    started: usize,
    active: u32,
    looked_up: bool,
    stats: Stats,
}

impl Session {
    /// A session of `plan` from `start`, its deadline `start` plus the plan's wall budget.
    pub fn new(plan: Plan, start: u64) -> Session {
        let deadline = start.saturating_add(plan.budget.wall_ns);
        let mut states = Vec::with_capacity_in(plan.candidates.len(), Global);
        for _ in 0..plan.candidates.len() {
            states.push(State {
                end: None,
                patched: BTreeSet::new_in(Global),
                next: 0,
                passed: Vec::new_in(Global),
            });
        }
        let mut session = Session {
            plan,
            now: start,
            deadline,
            out: VecDeque::new_in(Global),
            pending: BTreeMap::new_in(Global),
            next_ticket: 0,
            states,
            queue: Vec::new_in(Global),
            started: 0,
            active: 0,
            looked_up: false,
            stats: Stats::default(),
        };
        if session.plan.candidates.is_empty() {
            session.looked_up = true;
        } else {
            let mut keys = Vec::with_capacity_in(session.plan.candidates.len(), Global);
            for candidate in &session.plan.candidates {
                keys.push(candidate.key);
            }
            let ticket = session.ticket(Pending::Lookup);
            session.out.push_back(Effect::Lookup { ticket, keys });
        }
        session
    }

    /// The next effect to perform.
    pub fn take(&mut self) -> Option<Effect> {
        self.out.pop_front()
    }

    /// When the driver must call [`Session::advance`] if nothing completes first.
    pub fn next_deadline(&self) -> Option<u64> {
        (!self.is_done()).then_some(self.deadline)
    }

    /// The clock moved to `now` (a clock that goes back is held where it was).
    pub fn advance(&mut self, now: u64) {
        self.now = self.now.max(now);
        self.pump();
    }

    /// Every effect is done and nothing more will be asked.
    pub fn is_done(&self) -> bool {
        self.looked_up && self.pending.is_empty() && self.out.is_empty() && self.started == self.queue.len()
    }

    /// The effect under `ticket` ended with `completion`, observed at `now`.
    pub fn complete(&mut self, now: u64, ticket: Ticket, completion: Completion) -> Result<(), RunError> {
        self.now = self.now.max(now);
        let Some(pending) = self.pending.remove(&ticket) else {
            return Err(RunError::UnknownTicket { ticket });
        };
        match (pending, completion) {
            (Pending::Lookup, Completion::Found(values)) => self.found(values)?,
            (Pending::Lookup, _) => return Err(RunError::WrongCompletion { ticket, expected: "found" }),
            (Pending::Make(mutant), Completion::Made) => {
                self.stats.made += 1;
                self.step(mutant);
            }
            (Pending::Patch(mutant, program), Completion::Patched) => {
                self.stats.patched += 1;
                self.states[mutant as usize].patched.insert(program);
                self.step(mutant);
            }
            (Pending::Run(mutant, at, cut), Completion::Ran(outcome)) => {
                self.stats.runs += 1;
                let test = self.plan.tests[self.plan.candidates[mutant as usize].tests[at as usize] as usize].key;
                let killed = |how| Final::Verdict(Verdict::Killed { test, how }, Source::Ran);
                match outcome {
                    TestOutcome::Passed => {
                        let state = &mut self.states[mutant as usize];
                        state.passed.push(test);
                        state.next = at + 1;
                        self.step(mutant);
                    }
                    TestOutcome::Failed => self.end(mutant, killed(KilledBy::Failed)),
                    TestOutcome::Crashed => self.end(mutant, killed(KilledBy::Crashed)),
                    TestOutcome::TimedOut if cut => {
                        self.end(mutant, Final::Verdict(Verdict::NotRun(NotRun::Expired), Source::Ran))
                    }
                    TestOutcome::TimedOut => self.end(mutant, killed(KilledBy::Timeout)),
                }
            }
            (Pending::Make(mutant) | Pending::Patch(mutant, _) | Pending::Run(mutant, _, _), Completion::Expired) => {
                self.end(mutant, Final::Verdict(Verdict::NotRun(NotRun::Expired), Source::Ran))
            }
            (Pending::Make(mutant), Completion::Failed { what }) => {
                self.end(mutant, Final::Error(MutantError::Effect { stage: Stage::Make, what }))
            }
            (Pending::Patch(mutant, _), Completion::Failed { what }) => {
                self.end(mutant, Final::Error(MutantError::Effect { stage: Stage::Patch, what }))
            }
            (Pending::Run(mutant, _, _), Completion::Failed { what }) => {
                self.end(mutant, Final::Error(MutantError::Effect { stage: Stage::Run, what }))
            }
            (Pending::Make(_), _) => return Err(RunError::WrongCompletion { ticket, expected: "made" }),
            (Pending::Patch(..), _) => return Err(RunError::WrongCompletion { ticket, expected: "patched" }),
            (Pending::Run(..), _) => return Err(RunError::WrongCompletion { ticket, expected: "ran" }),
        }
        self.pump();
        Ok(())
    }

    /// The outcome, once [`Session::is_done`].
    pub fn finish(self) -> Result<Outcome, RunError> {
        if !self.is_done() {
            return Err(RunError::NotDone { pending: (self.pending.len() + self.out.len()) as u32 });
        }
        let mut verdicts = Vec::with_capacity_in(self.plan.candidates.len() + self.plan.decided.len(), Global);
        let mut errors = Vec::new_in(Global);
        for (candidate, state) in self.plan.candidates.iter().zip(self.states) {
            match state.end {
                Some(Final::Verdict(verdict, source)) => verdicts.push((candidate.at, verdict, source)),
                Some(Final::Error(error)) => errors.push((candidate.at, error)),
                // Not fitted, or not started before the deadline.
                None => verdicts.push((candidate.at, Verdict::NotRun(NotRun::Budget), Source::Static)),
            }
        }
        for decided in self.plan.decided {
            verdicts.push((decided.at, decided.verdict, Source::Static));
        }
        Ok(Outcome { verdicts, errors, stats: self.stats })
    }

    fn ticket(&mut self, pending: Pending) -> Ticket {
        let ticket = Ticket(self.next_ticket);
        self.next_ticket += 1;
        self.pending.insert(ticket, pending);
        ticket
    }

    fn found(&mut self, values: Vec<Option<Vec<u8, Global>>, Global>) -> Result<(), RunError> {
        if values.len() != self.plan.candidates.len() {
            return Err(RunError::LookupCount {
                keys: self.plan.candidates.len() as u32,
                values: values.len() as u32,
            });
        }
        let mut uncached = Vec::new_in(Global);
        let mut estimates = Vec::new_in(Global);
        for (index, value) in values.into_iter().enumerate() {
            match value {
                Some(bytes) => {
                    let end = match VerdictRecord::decode(&bytes) {
                        Ok(record) => {
                            self.stats.cached += 1;
                            Final::Verdict(record.verdict, Source::Cached)
                        }
                        Err(error) => Final::Error(MutantError::Decode(error)),
                    };
                    self.states[index].end = Some(end);
                }
                None => {
                    uncached.push(index as u32);
                    estimates.push(self.plan.candidates[index].estimate_ns);
                }
            }
        }
        let fits = fit(&estimates, &self.plan.budget);
        for (index, fits) in uncached.into_iter().zip(fits) {
            if fits {
                self.queue.push(index);
            }
        }
        self.looked_up = true;
        Ok(())
    }

    /// Start mutants while a slot is free and there is time.
    fn pump(&mut self) {
        if !self.looked_up {
            return;
        }
        if self.now >= self.deadline {
            // What was not started is not run, by the budget: [`Session::finish`] says so.
            self.queue.truncate(self.started);
            return;
        }
        let slots = self.plan.budget.slots.max(1);
        while self.active < slots && self.started < self.queue.len() {
            let mutant = self.queue[self.started];
            self.started += 1;
            self.active += 1;
            self.stats.peak = self.stats.peak.max(self.active);
            let candidate = &self.plan.candidates[mutant as usize];
            let (at, inliners) = (candidate.at, candidate.inliners.clone());
            let ticket = self.ticket(Pending::Make(mutant));
            self.out.push_back(Effect::Make { ticket, mutant: Mutant(mutant), at, inliners, deadline: self.deadline });
        }
    }

    /// The mutant's next effect: a patch of the next test's program, or the test, or the end.
    fn step(&mut self, mutant: u32) {
        if self.now >= self.deadline {
            return self.end(mutant, Final::Verdict(Verdict::NotRun(NotRun::Expired), Source::Ran));
        }
        let candidate = &self.plan.candidates[mutant as usize];
        let state = &self.states[mutant as usize];
        let Some(&test) = candidate.tests.get(state.next as usize) else {
            let mut tests = state.passed.clone();
            tests.sort_unstable();
            tests.dedup();
            return self.end(mutant, Final::Verdict(Verdict::Survived { tests }, Source::Ran));
        };
        let test = self.plan.tests[test as usize];
        if !state.patched.contains(&test.program) {
            let ticket = self.ticket(Pending::Patch(mutant, test.program));
            let deadline = self.deadline;
            self.out.push_back(Effect::Patch { ticket, mutant: Mutant(mutant), program: test.program, deadline });
            return;
        }
        let left = self.deadline - self.now;
        let cut = left < test.timeout_ns;
        let at = state.next;
        let ticket = self.ticket(Pending::Run(mutant, at, cut));
        self.out.push_back(Effect::Run {
            ticket,
            mutant: Mutant(mutant),
            test: test.key,
            program: test.program,
            timeout_ns: test.timeout_ns.min(left),
            deadline: self.deadline,
        });
    }

    fn end(&mut self, mutant: u32, end: Final) {
        if let Final::Verdict(verdict, Source::Ran) = &end {
            let record = VerdictRecord { verdict: verdict.clone() };
            if let Some(bytes) = record.encode() {
                let key = self.plan.candidates[mutant as usize].key;
                self.out.push_back(Effect::Put { key, bytes });
                self.stats.put += 1;
            }
        }
        self.states[mutant as usize].end = Some(end);
        self.out.push_back(Effect::Drop { mutant: Mutant(mutant) });
        self.active -= 1;
    }
}
