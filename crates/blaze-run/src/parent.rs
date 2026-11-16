//! [`Parent`]: the `blaze run` process's side of a running program. It is alive exactly as long
//! as the program, and holds nothing a record does not rebuild.
//!
//! - **The start.** [`Parent::start`] is given what must have code before the program's first
//!   instruction (the entry, and what runs before `main`: the start sections' initializers) and
//!   what the last run's record says ran. The first are wanted at once; [`Effect::Begin`] goes
//!   out when each is published, [`Effect::NoStart`] when one has no code.
//! - **A need.** A thread called a stub whose record the program does not hold. The function is
//!   looked up by key ([`Effect::Lookup`]); found, it is published; absent, compiled by the
//!   [`Compiler`] ([`Effect::Compile`]), then published. However many threads ask, and however
//!   often, it is looked up once and compiled at most once (single flight), and each asker is
//!   answered with the same outcome after the publish.
//! - **Speculation.** The callees of each function published, and what the last run ran, are
//!   wanted at lower priorities ([`Priority`]). A background compile never takes the last free
//!   worker, so a thread's own function never waits behind speculation.
//! - **Failure (rule 20).** A function that does not compile fails every caller with its typed
//!   [`CompileError`]; nothing else stops. The run goes on, and its other functions are served.
//! - **Bounds (rule 18).** A compile runs against a deadline on the driver's clock
//!   ([`Config::compile_bound`]); past it, its askers are failed with [`Why::Overran`]. Waiters
//!   are capped ([`Config::most_waiters`]).
//!
//! Mirrors `blaze_lazy::fetch::Fetcher` (the daemon-era single flight, priorities and the worker
//! held back for asks), re-seated: one program, records in place of the daemon's memory, and a
//! lookup before every compile.

#![blaze::sans_io]

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;

use crate::registry::RunEvent;
use crate::tier::{Identities, Inlined, usable_planted};
use crate::{CodeId, CodeRef, CompileError, InstanceKey, Nanos, Plant, SlotId, ThreadId, Tier, Why};

/// What a compile is for, the most urgent first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Priority {
    /// A thread waits for it, or the start does.
    Asked = 0,
    /// A function just published calls it.
    Callee = 1,
    /// The last run ran it.
    History = 2,
}

const PRIORITIES: [Priority; 3] = [Priority::Asked, Priority::Callee, Priority::History];

/// One function of the program's manifest, by slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Function {
    pub instance: InstanceKey,
    /// Its body's code identity in this snapshot.
    pub code: CodeId,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// Compiles at once; at least 2, so one is always free for a thread's own function.
    pub workers: u32,
    /// How long a compile may run on the driver's clock before its askers are failed.
    pub compile_bound: Nanos,
    /// The most asks waiting at once.
    pub most_waiters: u32,
}

/// One fact a compile read and the digest it had: the record's read list (read-addressed.md).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Read {
    pub fact: [u8; 32],
    pub digest: [u8; 32],
}

/// What the compiler made of one function: the fast-tier code record (written to the store by
/// the compile's task before it ends), the slots its code calls through, and its read list.
pub struct FastRecord<A: Allocator> {
    pub code: CodeRef,
    pub callees: Vec<SlotId, A>,
    pub reads: Vec<Read, A>,
}

/// The fast tier's compiler: the type checker and codegen behind one call (the real one comes
/// from the new item layer, bottom.md). Its driver calls it for each [`Effect::Compile`].
pub trait Compiler<A: Allocator + Clone> {
    fn compile(&mut self, instance: InstanceKey, alloc: A) -> Result<FastRecord<A>, Why>;
}

/// What a lookup found in the store: the fast record by key, and the optimized one by its
/// address, if either is there.
#[derive(Clone, Copy, Debug)]
pub struct Found<'a> {
    pub fast: CodeRef,
    pub callees: &'a [SlotId],
    pub optimized: Option<FoundOptimized<'a>>,
}

#[derive(Clone, Copy, Debug)]
pub struct FoundOptimized<'a> {
    pub code: CodeRef,
    pub inlined: &'a [Inlined],
}

/// The answer a waiting thread is given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    /// The slot's word names code: call through it again.
    Code { tier: Tier },
    /// The function has no code: the call fails with this.
    Failed(CompileError),
}

/// What the parent asks its driver to do, in order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Look `instance`'s records up, then call [`Parent::looked`].
    Lookup { slot: SlotId, instance: InstanceKey },
    /// Run the compiler on a worker, then call [`Parent::compiled`]. Past `deadline` the
    /// parent's [`Parent::tick`] fails its askers.
    Compile { slot: SlotId, instance: InstanceKey, priority: Priority, deadline: Nanos },
    /// Tell the program to place `code` in `slot` at `tier`.
    Publish { slot: SlotId, tier: Tier, code: CodeRef },
    /// Wake `thread`, which waits on `slot`.
    Answer { thread: ThreadId, slot: SlotId, answer: Answer },
    /// Everything the start needs is published: run the entry.
    Begin,
    /// A function the start needs has no code: the program does not start.
    NoStart(CompileError),
    /// Only under a planted bug: the run stopped at a failed compile.
    Stop(CompileError),
    /// For the run's event stream.
    Say(RunEvent),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    Config { what: &'static str, value: u64 },
    NoSlot { slot: SlotId, slots: u32 },
    TooManyWaiters { most: u32, slot: SlotId },
    /// A lookup's answer for a slot that was not being looked up.
    NotLooking { slot: SlotId },
    /// A compile's end for a slot that was not compiling.
    NotCompiling { slot: SlotId },
    StartedTwice,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Config { what, value } => write!(f, "the parent's config: {what} ({value})"),
            Error::NoSlot { slot, slots } => {
                write!(f, "the program asked for slot {}, past its {slots}", slot.0)
            }
            Error::TooManyWaiters { most, slot } => {
                write!(f, "more than {most} asks waiting at once, at slot {}", slot.0)
            }
            Error::NotLooking { slot } => {
                write!(f, "a lookup ended for slot {}, which was not being looked up", slot.0)
            }
            Error::NotCompiling { slot } => {
                write!(f, "a compile ended for slot {}, which was not compiling", slot.0)
            }
            Error::StartedTwice => write!(f, "the program was started twice"),
        }
    }
}

impl core::error::Error for Error {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Unasked,
    Looking { priority: Priority },
    Queued { priority: Priority },
    Compiling { priority: Priority, deadline: Nanos },
    Ready { tier: Tier },
    /// No code. `compiling` while an overrun compile still holds its worker.
    Failed { error: CompileError, compiling: Option<Priority> },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Start {
    NotYet,
    Waiting { left: u32 },
    Begun,
    Refused,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Asker {
    Start,
    Thread(ThreadId),
}

/// One program's parent. See the module's documentation.
pub struct Parent<A: Allocator + Clone> {
    config: Config,
    functions: Vec<Function, A>,
    ids: BTreeMap<InstanceKey, CodeId, A>,
    state: Vec<State, A>,
    /// Who waits for which slot, in the order they asked.
    waiters: Vec<(SlotId, Asker), A>,
    queues: [VecDeque<SlotId, A>; 3],
    running: u32,
    running_background: u32,
    start: Start,
    now: Nanos,
    compiles: u64,
    pub(crate) plant: Plant,
}

impl<A: Allocator + Clone> Identities for Parent<A> {
    fn code_id(&self, instance: InstanceKey) -> Option<CodeId> {
        self.ids.get(&instance).copied()
    }
}

impl<A: Allocator + Clone> Parent<A> {
    /// The parent of a program whose slots run `functions`, in `alloc`.
    pub fn new(functions: &[Function], config: Config, alloc: A) -> Result<Parent<A>, Error> {
        if config.workers < 2 {
            return Err(Error::Config {
                what: "fewer than 2 workers: none would be kept for a thread's own function",
                value: u64::from(config.workers),
            });
        }
        if config.compile_bound.0 == 0 || config.most_waiters == 0 {
            return Err(Error::Config { what: "a bound of 0", value: 0 });
        }
        let mut all = Vec::with_capacity_in(functions.len(), alloc.clone());
        all.extend_from_slice(functions);
        let mut ids = BTreeMap::new_in(alloc.clone());
        for function in functions {
            ids.insert(function.instance, function.code);
        }
        let mut state = Vec::with_capacity_in(functions.len(), alloc.clone());
        state.resize(functions.len(), State::Unasked);
        Ok(Parent {
            config,
            functions: all,
            ids,
            state,
            waiters: Vec::new_in(alloc.clone()),
            queues: [
                VecDeque::new_in(alloc.clone()),
                VecDeque::new_in(alloc.clone()),
                VecDeque::new_in(alloc),
            ],
            running: 0,
            running_background: 0,
            start: Start::NotYet,
            now: Nanos(0),
            compiles: 0,
            plant: Plant::None,
        })
    }

    fn slots(&self) -> u32 {
        self.functions.len() as u32
    }

    fn check(&self, slot: SlotId) -> Result<(), Error> {
        match slot.index() < self.functions.len() {
            true => Ok(()),
            false => Err(Error::NoSlot { slot, slots: self.slots() }),
        }
    }

    pub fn function(&self, slot: SlotId) -> Option<Function> {
        self.functions.get(slot.index()).copied()
    }

    /// How many compiles were started: at most one per function.
    pub fn compiles(&self) -> u64 {
        self.compiles
    }

    /// Whether `slot` has code, and at which tier.
    pub fn ready(&self, slot: SlotId) -> Option<Tier> {
        match self.state.get(slot.index()) {
            Some(State::Ready { tier }) => Some(*tier),
            _ => None,
        }
    }

    /// The start: `needed` must have code before the first instruction; `history` ran last time.
    pub fn start(
        &mut self,
        needed: &[SlotId],
        history: &[SlotId],
        now: Nanos,
        out: &mut Vec<Effect, A>,
    ) -> Result<(), Error> {
        if self.start != Start::NotYet {
            return Err(Error::StartedTwice);
        }
        for slot in needed.iter().chain(history) {
            self.check(*slot)?;
        }
        self.now = now;
        self.start = Start::Waiting { left: 0 };
        for slot in needed {
            if self.waiters.iter().any(|it| *it == (*slot, Asker::Start)) {
                continue;
            }
            match self.state[slot.index()] {
                State::Ready { .. } => continue,
                State::Failed { error, .. } => {
                    self.refuse_start(error, out);
                    return Ok(());
                }
                _ => {}
            }
            self.wait(*slot, Asker::Start)?;
            if let Start::Waiting { left } = &mut self.start {
                *left += 1;
            }
            self.want(*slot, Priority::Asked, out);
        }
        for slot in history {
            self.want(*slot, Priority::History, out);
        }
        self.begin_if_ready(out);
        self.pump(out);
        Ok(())
    }

    /// `thread` called `slot`'s stub.
    pub fn need(
        &mut self,
        thread: ThreadId,
        slot: SlotId,
        now: Nanos,
        out: &mut Vec<Effect, A>,
    ) -> Result<(), Error> {
        self.check(slot)?;
        self.now = self.now.max(now);
        match self.state[slot.index()] {
            State::Ready { tier } => {
                out.push(Effect::Answer { thread, slot, answer: Answer::Code { tier } });
            }
            State::Failed { error, .. } => {
                out.push(Effect::Answer { thread, slot, answer: Answer::Failed(error) });
            }
            State::Compiling { priority, deadline } if self.plant == Plant::CompileTwice => {
                self.wait(slot, Asker::Thread(thread))?;
                let instance = self.functions[slot.index()].instance;
                out.push(Effect::Compile { slot, instance, priority, deadline });
            }
            State::Unasked | State::Looking { .. } | State::Queued { .. } | State::Compiling { .. } => {
                self.wait(slot, Asker::Thread(thread))?;
                self.want(slot, Priority::Asked, out);
            }
        }
        self.pump(out);
        Ok(())
    }

    fn wait(&mut self, slot: SlotId, asker: Asker) -> Result<(), Error> {
        if self.waiters.contains(&(slot, asker)) {
            return Ok(());
        }
        if self.waiters.len() >= self.config.most_waiters as usize {
            return Err(Error::TooManyWaiters { most: self.config.most_waiters, slot });
        }
        self.waiters.push((slot, asker));
        Ok(())
    }

    /// Wants `slot` at `priority`: looked up first, or moved up in its queue.
    fn want(&mut self, slot: SlotId, priority: Priority, out: &mut Vec<Effect, A>) {
        match self.state[slot.index()] {
            State::Unasked => {
                self.state[slot.index()] = State::Looking { priority };
                let instance = self.functions[slot.index()].instance;
                out.push(Effect::Lookup { slot, instance });
            }
            State::Looking { priority: was } if priority < was => {
                self.state[slot.index()] = State::Looking { priority };
            }
            State::Queued { priority: was } if priority < was => {
                self.state[slot.index()] = State::Queued { priority };
                self.queues[priority as usize].push_back(slot);
            }
            State::Looking { .. }
            | State::Queued { .. }
            | State::Compiling { .. }
            | State::Ready { .. }
            | State::Failed { .. } => {}
        }
    }

    /// The store's answer to [`Effect::Lookup`] for `slot`.
    pub fn looked(
        &mut self,
        slot: SlotId,
        found: Option<Found<'_>>,
        out: &mut Vec<Effect, A>,
    ) -> Result<(), Error> {
        self.check(slot)?;
        let State::Looking { priority } = self.state[slot.index()] else {
            return Err(Error::NotLooking { slot });
        };
        match found {
            Some(found) => {
                let optimized = found
                    .optimized
                    .filter(|it| usable_planted(it.inlined, &*self, self.plant).is_ok());
                let (tier, code) = match optimized {
                    Some(optimized) => (Tier::Optimized, optimized.code),
                    None => (Tier::Fast, found.fast),
                };
                self.publish(slot, tier, code, found.callees, out);
            }
            None => {
                self.state[slot.index()] = State::Queued { priority };
                self.queues[priority as usize].push_back(slot);
            }
        }
        self.pump(out);
        Ok(())
    }

    /// The end of [`Effect::Compile`] for `slot`.
    pub fn compiled(
        &mut self,
        slot: SlotId,
        result: Result<&FastRecord<A>, Why>,
        now: Nanos,
        out: &mut Vec<Effect, A>,
    ) -> Result<(), Error> {
        self.check(slot)?;
        self.now = self.now.max(now);
        let (priority, late) = match self.state[slot.index()] {
            State::Compiling { priority, .. } => (priority, false),
            State::Failed { compiling: Some(priority), error } => {
                self.state[slot.index()] = State::Failed { error, compiling: None };
                (priority, true)
            }
            _ => return Err(Error::NotCompiling { slot }),
        };
        self.running -= 1;
        if priority != Priority::Asked {
            self.running_background -= 1;
        }
        match result {
            // A late success still serves every later caller; the early ones were failed.
            Ok(record) => self.publish(slot, Tier::Fast, record.code, &record.callees, out),
            Err(_) if late => {}
            Err(why) => {
                let error = CompileError { function: self.functions[slot.index()].instance, slot, why };
                self.fail(slot, error, out);
            }
        }
        self.pump(out);
        Ok(())
    }

    /// The driver's clock: every compile past its deadline fails its askers.
    pub fn tick(&mut self, now: Nanos, out: &mut Vec<Effect, A>) {
        self.now = self.now.max(now);
        for at in 0..self.state.len() {
            let State::Compiling { priority, deadline } = self.state[at] else { continue };
            if deadline > self.now {
                continue;
            }
            let slot = SlotId(at as u32);
            let why = Why::Overran { bound: self.config.compile_bound };
            let error = CompileError { function: self.functions[at].instance, slot, why };
            self.fail(slot, error, out);
            self.state[at] = State::Failed { error, compiling: Some(priority) };
        }
        self.pump(out);
    }

    fn publish(
        &mut self,
        slot: SlotId,
        tier: Tier,
        code: CodeRef,
        callees: &[SlotId],
        out: &mut Vec<Effect, A>,
    ) {
        self.state[slot.index()] = State::Ready { tier };
        let function = self.functions[slot.index()].instance;
        out.push(Effect::Publish { slot, tier, code });
        out.push(Effect::Say(match tier {
            Tier::Fast => RunEvent::Published { function },
            Tier::Optimized => RunEvent::Tiered { function },
        }));
        self.answer(slot, Answer::Code { tier }, out);
        for callee in callees {
            if callee.index() < self.functions.len() {
                self.want(*callee, Priority::Callee, out);
            }
        }
    }

    fn fail(&mut self, slot: SlotId, error: CompileError, out: &mut Vec<Effect, A>) {
        self.state[slot.index()] = State::Failed { error, compiling: None };
        out.push(Effect::Say(RunEvent::Failed { function: error.function }));
        self.answer(slot, Answer::Failed(error), out);
        if self.plant == Plant::StopOnFailure {
            out.push(Effect::Stop(error));
        }
    }

    /// Answers `slot`'s waiters in the order they asked, and the start if it waited.
    fn answer(&mut self, slot: SlotId, answer: Answer, out: &mut Vec<Effect, A>) {
        let mut start = false;
        self.waiters.retain(|(waits, asker)| {
            if *waits != slot {
                return true;
            }
            match asker {
                Asker::Thread(thread) => out.push(Effect::Answer { thread: *thread, slot, answer }),
                Asker::Start => start = true,
            }
            false
        });
        if !start {
            return;
        }
        match answer {
            Answer::Code { .. } => {
                if let Start::Waiting { left } = &mut self.start {
                    *left -= 1;
                }
                self.begin_if_ready(out);
            }
            Answer::Failed(error) => self.refuse_start(error, out),
        }
    }

    fn begin_if_ready(&mut self, out: &mut Vec<Effect, A>) {
        if self.start == (Start::Waiting { left: 0 }) {
            self.start = Start::Begun;
            out.push(Effect::Begin);
        }
    }

    fn refuse_start(&mut self, error: CompileError, out: &mut Vec<Effect, A>) {
        if matches!(self.start, Start::Waiting { .. }) {
            self.start = Start::Refused;
            self.waiters.retain(|(_, asker)| *asker != Asker::Start);
            out.push(Effect::NoStart(error));
        }
    }

    /// Starts what the workers may take: asks on any free worker, background work only while
    /// one worker stays free.
    fn pump(&mut self, out: &mut Vec<Effect, A>) {
        for priority in PRIORITIES {
            let background = priority != Priority::Asked;
            loop {
                if self.running >= self.config.workers
                    || (background && self.running_background >= self.config.workers - 1)
                {
                    break;
                }
                let Some(slot) = self.queues[priority as usize].pop_front() else { break };
                if self.state[slot.index()] != (State::Queued { priority }) {
                    continue;
                }
                let deadline = self.now.after(self.config.compile_bound);
                self.state[slot.index()] = State::Compiling { priority, deadline };
                self.running += 1;
                self.running_background += u32::from(background);
                self.compiles += 1;
                let instance = self.functions[slot.index()].instance;
                out.push(Effect::Compile { slot, instance, priority, deadline });
                out.push(Effect::Say(RunEvent::Compiling { function: instance }));
            }
        }
    }
}
