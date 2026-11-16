//! [`Parent`]: the `blaze run` process's side of a running program. It is alive exactly as long
//! as the program, and holds nothing a record does not rebuild.
//!
//! The unit of work is a slot's function. Its code record is keyed by [`CodeKey`] (function,
//! tier) and holds no instrumentation, so each is made once and shared by every run whatever
//! any run observes.
//!
//! - **The start.** [`Parent::start`] is given what must have code before the program's first
//!   instruction (the entry, and what runs before `main`: the start sections' initializers) and
//!   what the last run's record says ran. The first are wanted at once; [`Effect::Begin`] goes
//!   out when each is published, [`Effect::NoStart`] when one has no code.
//! - **A need.** A thread called a need stub whose record the program does not hold. The slot's
//!   record is looked up by key ([`Effect::Lookup`]); found, it is published; absent, compiled
//!   by the [`Compiler`] ([`Effect::Compile`]), then published. However many threads ask, and
//!   however often, one compile of a function is in flight at a time (single flight), and each
//!   asker is answered with the same outcome after the publish.
//! - **No compile waits.** A compile returns its record or the records it lacks
//!   ([`Compiled::Need`]). The function is then blocked and holds no worker; each lacking record
//!   is made once however many compiles lack it ([`Effect::Make`], [`Parent::made`]), and the
//!   compile is entered again from the top once it lacks nothing.
//! - **Speculation.** The callees of each function published, and what the last run ran, are
//!   wanted at lower priorities ([`Priority`]). A background compile never takes the last free
//!   worker, so a thread's own function never waits behind speculation.
//! - **Hooks.** The viewer turns hooks on or off for a function or a set ([`Parent::toggle`]):
//!   each slot's hooks are kept here and sent as [`Effect::Hooks`], which the program's control
//!   thread makes a hook stub in front of the code (`Slots::observe`, `Slots::plain`). Nothing is
//!   compiled, so a toggle cannot fail and takes effect at the slot's next call, before its code
//!   exists too (the stub then jumps to the need stub).
//! - **Failure (rule 20).** A function that does not compile fails every caller with its typed
//!   [`CompileError`]; nothing else stops. The run goes on.
//! - **Bounds (rule 18).** A compile, with its re-entries, runs against one deadline on the
//!   driver's clock ([`Config::compile_bound`]); past it, its askers are failed with
//!   [`Why::Overran`]. Waiters are capped ([`Config::most_waiters`]).
//!
//! Mirrors `blaze_lazy::fetch::Fetcher` (the daemon-era single flight, priorities and the worker
//! held back for asks), re-seated: one program, records in place of the daemon's memory, and a
//! lookup before every compile.

#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{SortedMap, Vec, VecDeque, soa};

use crate::registry::RunEvent;
use crate::tier::{Identities, Inlined, usable_planted};
use crate::{
    CodeId, CodeKey, CodeRef, CompileError, Hooks, InstanceKey, Made, Nanos, Plant, RecordKey,
    SlotId, ThreadId, Tier, Why,
};

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
    /// How long a compile, with its re-entries, may take on the driver's clock before its
    /// askers are failed.
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

/// What the compiler made of one function: its fast-tier code record (written to the store by
/// the compile's task before it ends), the slots its code calls through, and its read list.
pub struct FastRecord<A: Allocator> {
    pub code: CodeRef,
    pub callees: Vec<SlotId, A>,
    pub reads: Vec<Read, A>,
}

/// The fast tier's compiler: the type checker and codegen behind one call (the real one comes
/// from the new item layer, bottom.md). A pure function of what it reads: it returns its record
/// or the records it lacks, and never waits. Its driver calls it for each [`Effect::Compile`].
pub trait Compiler<A: Allocator + Clone> {
    fn compile(&mut self, key: CodeKey, alloc: A) -> Result<Made<FastRecord<A>, A>, Why>;
}

/// How one entry of a compile ended, as its driver hands it back.
pub enum Compiled<'r, A: Allocator> {
    Record(&'r FastRecord<A>),
    Need(&'r [RecordKey]),
    Failed(Why),
}

/// What a lookup found in the store: the function's fast record by key, and the optimized one
/// by its address, if either is there.
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
    /// The slot's body word names code: call through the slot again.
    Code { tier: Tier },
    /// The function has no code: the call fails with this.
    Failed(CompileError),
}

/// What the parent asks its driver to do, in order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Look `key`'s records up, then call [`Parent::looked`].
    Lookup { slot: SlotId, key: CodeKey },
    /// Run the compiler on a worker, then call [`Parent::compiled`]. Past `deadline` the
    /// parent's [`Parent::tick`] fails its askers.
    Compile { slot: SlotId, key: CodeKey, priority: Priority, deadline: Nanos },
    /// Make the record `key` (by the store or by its task), then call [`Parent::made`].
    Make { key: RecordKey, priority: Priority },
    /// Tell the program to place `code` as `slot`'s body (`Slots::publish`).
    Publish { slot: SlotId, tier: Tier, code: CodeRef },
    /// Tell the program `slot` is to run with `hooks` (all of them, not a change): its hook stub
    /// goes in or out (`Slots::observe`, `Slots::plain`).
    Hooks { slot: SlotId, hooks: Hooks },
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
    /// A `Need` that names nothing: the compile could never be entered again.
    EmptyNeed { slot: SlotId },
    /// A record made that no compile lacked.
    NotNeeded { key: RecordKey },
    /// Optimized code for a slot with no fast code.
    NotReady { slot: SlotId },
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
            Error::EmptyNeed { slot } => {
                write!(f, "the compile of slot {} lacked nothing and returned no record", slot.0)
            }
            Error::NotNeeded { key } => {
                write!(f, "record {:02x?} was made and no compile lacked it", &key.0[..8])
            }
            Error::NotReady { slot } => {
                write!(f, "slot {} was tiered before it had fast code", slot.0)
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
    Queued { priority: Priority, deadline: Option<Nanos> },
    Compiling { priority: Priority, deadline: Nanos },
    /// Waits for `left` records to be made; holds no worker.
    Blocked { priority: Priority, deadline: Nanos, left: u32 },
    Ready { tier: Tier, code: CodeRef },
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

soa! {
    /// One slot as the parent keeps it: the function it runs, its code's state, and the hooks
    /// the viewer asked for. By columns: the clock's pass reads `states` alone.
    struct Row, Rows, RowRef {
        function: Function => functions / functions_mut,
        state: State => states / states_mut,
        hooks: Hooks => hooks / hooks_mut,
    }
}

/// One program's parent. See the module's documentation.
pub struct Parent<A: Allocator + Clone> {
    alloc: A,
    config: Config,
    /// A row a slot, by columns ([`Row`]).
    table: Rows<A>,
    /// Each instance's code identity, sorted once at the start and never inserted into.
    ids: SortedMap<InstanceKey, CodeId, A>,
    /// Who waits for which slot, in the order they asked.
    waiters: Vec<(SlotId, Asker), A>,
    queues: [VecDeque<SlotId, A>; 3],
    /// Each record being made, and the slots whose compiles lack it: the few in flight.
    making: SortedMap<RecordKey, Vec<SlotId, A>, A>,
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
        let n = functions.len();
        let mut table = Rows::with_capacity_in(n, alloc.clone());
        for function in functions {
            table.push(Row { function: *function, state: State::Unasked, hooks: Hooks::NONE });
        }
        let mut pairs = Vec::with_capacity_in(n, alloc.clone());
        pairs.extend(functions.iter().map(|function| (function.instance, function.code)));
        pairs.sort_unstable_by_key(|(instance, _)| *instance);
        let ids = SortedMap::from_sorted(pairs)
            .map_err(|twice| Error::Config { what: "two slots run one instance", value: twice.at as u64 })?;
        Ok(Parent {
            config,
            table,
            ids,
            waiters: Vec::new_in(alloc.clone()),
            queues: [VecDeque::new_in(alloc.clone()), VecDeque::new_in(alloc.clone()), VecDeque::new_in(alloc.clone())],
            making: SortedMap::new_in(alloc.clone()),
            alloc,
            running: 0,
            running_background: 0,
            start: Start::NotYet,
            now: Nanos(0),
            compiles: 0,
            plant: Plant::None,
        })
    }

    fn slots(&self) -> u32 {
        self.table.len() as u32
    }

    fn instance(&self, slot: SlotId) -> InstanceKey {
        self.table.functions()[slot.index()].instance
    }

    fn check(&self, slot: SlotId) -> Result<(), Error> {
        match slot.index() < self.table.len() {
            true => Ok(()),
            false => Err(Error::NoSlot { slot, slots: self.slots() }),
        }
    }

    fn state(&self, slot: SlotId) -> State {
        self.table.states()[slot.index()]
    }

    fn set(&mut self, slot: SlotId, state: State) {
        self.table.states_mut()[slot.index()] = state;
    }

    fn key(&self, slot: SlotId) -> CodeKey {
        CodeKey { function: self.instance(slot), tier: Tier::Fast }
    }

    pub fn function(&self, slot: SlotId) -> Option<Function> {
        self.table.functions().get(slot.index()).copied()
    }

    /// How many compile entries were started (re-entries after a `Need` included).
    pub fn compiles(&self) -> u64 {
        self.compiles
    }

    /// Whether `slot` has code, and at which tier.
    pub fn ready(&self, slot: SlotId) -> Option<Tier> {
        match self.table.states().get(slot.index()) {
            Some(State::Ready { tier, .. }) => Some(*tier),
            _ => None,
        }
    }

    /// The hooks `slot` is to run with.
    pub fn hooks(&self, slot: SlotId) -> Option<Hooks> {
        self.table.hooks().get(slot.index()).copied()
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
            match self.state(*slot) {
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

    /// `thread` called `slot`'s need stub.
    pub fn need(
        &mut self,
        thread: ThreadId,
        slot: SlotId,
        now: Nanos,
        out: &mut Vec<Effect, A>,
    ) -> Result<(), Error> {
        self.check(slot)?;
        self.now = self.now.max(now);
        match self.state(slot) {
            State::Ready { tier, .. } => {
                out.push(Effect::Answer { thread, slot, answer: Answer::Code { tier } });
            }
            State::Failed { error, .. } => {
                out.push(Effect::Answer { thread, slot, answer: Answer::Failed(error) });
            }
            State::Compiling { priority, deadline } if self.plant == Plant::CompileTwice => {
                self.wait(slot, Asker::Thread(thread))?;
                let key = self.key(slot);
                out.push(Effect::Compile { slot, key, priority, deadline });
            }
            State::Unasked
            | State::Looking { .. }
            | State::Queued { .. }
            | State::Compiling { .. }
            | State::Blocked { .. } => {
                self.wait(slot, Asker::Thread(thread))?;
                self.want(slot, Priority::Asked, out);
            }
        }
        self.pump(out);
        Ok(())
    }

    /// The viewer turned `hooks` on or off for each of `slots`: each slot whose hooks change is
    /// sent its whole set. Nothing is compiled.
    pub fn toggle(
        &mut self,
        slots: &[SlotId],
        hooks: Hooks,
        on: bool,
        out: &mut Vec<Effect, A>,
    ) -> Result<(), Error> {
        for slot in slots {
            self.check(*slot)?;
        }
        for slot in slots {
            let was = self.table.hooks()[slot.index()];
            let now = was.with(hooks, on);
            if now == was || (now.is_none() && self.plant == Plant::OffKeepsHooks) {
                continue;
            }
            self.table.hooks_mut()[slot.index()] = now;
            out.push(Effect::Hooks { slot: *slot, hooks: now });
        }
        Ok(())
    }

    /// The tier made optimized `code` for `slot`: its code from now, published at once (a stub
    /// in front of it stays in front of the new body).
    pub fn tiered(&mut self, slot: SlotId, code: CodeRef, out: &mut Vec<Effect, A>) -> Result<(), Error> {
        self.check(slot)?;
        let State::Ready { .. } = self.state(slot) else {
            return Err(Error::NotReady { slot });
        };
        self.set(slot, State::Ready { tier: Tier::Optimized, code });
        out.push(Effect::Publish { slot, tier: Tier::Optimized, code });
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
        match self.state(slot) {
            State::Unasked => {
                self.set(slot, State::Looking { priority });
                let key = self.key(slot);
                out.push(Effect::Lookup { slot, key });
            }
            State::Looking { priority: was } if priority < was => {
                self.set(slot, State::Looking { priority });
            }
            State::Queued { priority: was, deadline } if priority < was => {
                self.set(slot, State::Queued { priority, deadline });
                self.queues[priority as usize].push_back(slot);
            }
            State::Looking { .. }
            | State::Queued { .. }
            | State::Compiling { .. }
            | State::Blocked { .. }
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
        let State::Looking { priority } = self.state(slot) else {
            return Err(Error::NotLooking { slot });
        };
        match found {
            Some(found) => {
                let optimized =
                    found.optimized.filter(|it| usable_planted(it.inlined, &*self, self.plant).is_ok());
                let (tier, code) = match optimized {
                    Some(optimized) => (Tier::Optimized, optimized.code),
                    None => (Tier::Fast, found.fast),
                };
                self.has_code(slot, tier, code, found.callees, out);
            }
            None => {
                self.set(slot, State::Queued { priority, deadline: None });
                self.queues[priority as usize].push_back(slot);
            }
        }
        self.pump(out);
        Ok(())
    }

    /// The end of one entry of [`Effect::Compile`] for `slot`.
    pub fn compiled(
        &mut self,
        slot: SlotId,
        ended: Compiled<'_, A>,
        now: Nanos,
        out: &mut Vec<Effect, A>,
    ) -> Result<(), Error> {
        self.check(slot)?;
        self.now = self.now.max(now);
        let (priority, deadline, late) = match self.state(slot) {
            State::Compiling { priority, deadline } => (priority, deadline, false),
            State::Failed { compiling: Some(priority), error } => {
                self.set(slot, State::Failed { error, compiling: None });
                (priority, self.now, true)
            }
            _ => return Err(Error::NotCompiling { slot }),
        };
        self.running -= 1;
        if priority != Priority::Asked {
            self.running_background -= 1;
        }
        match ended {
            // A late success still serves every later caller; the early ones were failed.
            Compiled::Record(record) => {
                self.has_code(slot, Tier::Fast, record.code, &record.callees, out);
            }
            Compiled::Need(_) | Compiled::Failed(_) if late => {}
            Compiled::Need(keys) => {
                if keys.is_empty() {
                    return Err(Error::EmptyNeed { slot });
                }
                let mut left = 0;
                for key in keys {
                    match self.making.get_mut(key) {
                        Some(slots) if slots.contains(&slot) => continue,
                        Some(slots) => slots.push(slot),
                        None => {
                            let mut slots = Vec::new_in(self.alloc.clone());
                            slots.push(slot);
                            self.making.insert(*key, slots);
                            out.push(Effect::Make { key: *key, priority });
                        }
                    }
                    left += 1;
                }
                self.set(slot, State::Blocked { priority, deadline, left });
            }
            Compiled::Failed(why) => {
                let error = CompileError { function: self.instance(slot), slot, why };
                self.fail(slot, error, out);
            }
        }
        self.pump(out);
        Ok(())
    }

    /// The record `key` was made (or could not be): each compile that lacked it is entered again
    /// once it lacks nothing, or fails with the record's error.
    pub fn made(&mut self, key: RecordKey, result: Result<(), Why>, out: &mut Vec<Effect, A>) -> Result<(), Error> {
        let Some(slots) = self.making.remove(&key) else {
            return Err(Error::NotNeeded { key });
        };
        for slot in slots {
            let State::Blocked { priority, deadline, left } = self.state(slot) else { continue };
            match result {
                Err(why) => {
                    let function = self.instance(slot);
                    self.fail(slot, CompileError { function, slot, why }, out);
                }
                Ok(()) if left > 1 && self.plant != Plant::ReenterEarly => {
                    self.set(slot, State::Blocked { priority, deadline, left: left - 1 });
                }
                Ok(()) => {
                    self.set(slot, State::Queued { priority, deadline: Some(deadline) });
                    self.queues[priority as usize].push_front(slot);
                }
            }
        }
        self.pump(out);
        Ok(())
    }

    /// The driver's clock: every compile past its deadline fails its askers.
    pub fn tick(&mut self, now: Nanos, out: &mut Vec<Effect, A>) {
        self.now = self.now.max(now);
        let mut over = Vec::new_in(self.alloc.clone());
        for (at, state) in self.table.states().iter().enumerate() {
            let (deadline, holds) = match *state {
                State::Compiling { priority, deadline } => (deadline, Some(priority)),
                State::Blocked { deadline, .. } => (deadline, None),
                State::Queued { deadline: Some(deadline), .. } => (deadline, None),
                _ => continue,
            };
            if deadline <= self.now {
                over.push((SlotId(at as u32), holds));
            }
        }
        for (slot, holds) in over {
            let why = Why::Overran { bound: self.config.compile_bound };
            let error = CompileError { function: self.instance(slot), slot, why };
            self.fail(slot, error, out);
            self.set(slot, State::Failed { error, compiling: holds });
        }
        self.pump(out);
    }

    /// `slot` has code: kept, published, its askers answered, its callees wanted.
    fn has_code(&mut self, slot: SlotId, tier: Tier, code: CodeRef, callees: &[SlotId], out: &mut Vec<Effect, A>) {
        self.set(slot, State::Ready { tier, code });
        let function = self.instance(slot);
        out.push(Effect::Say(match tier {
            Tier::Fast => RunEvent::Published { function },
            Tier::Optimized => RunEvent::Tiered { function },
        }));
        out.push(Effect::Publish { slot, tier, code });
        self.answer(slot, Answer::Code { tier }, out);
        for callee in callees {
            if callee.index() < self.table.len() {
                self.want(*callee, Priority::Callee, out);
            }
        }
    }

    fn fail(&mut self, slot: SlotId, error: CompileError, out: &mut Vec<Effect, A>) {
        self.set(slot, State::Failed { error, compiling: None });
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
                let State::Queued { priority: at, deadline } = self.state(slot) else { continue };
                if at != priority {
                    continue;
                }
                let deadline = deadline.unwrap_or(self.now.after(self.config.compile_bound));
                self.set(slot, State::Compiling { priority, deadline });
                self.running += 1;
                self.running_background += u32::from(background);
                self.compiles += 1;
                let key = self.key(slot);
                out.push(Effect::Compile { slot, key, priority, deadline });
                out.push(Effect::Say(RunEvent::Compiling { function: key.function }));
            }
        }
    }
}
