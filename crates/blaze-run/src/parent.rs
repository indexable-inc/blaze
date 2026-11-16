//! [`Parent`]: the `blaze run` process's side of a running program. It is alive exactly as long
//! as the program, and holds nothing a record does not rebuild.
//!
//! The unit of work is a [`Version`]: one slot's function with a set of [`Hooks`] (none for the
//! plain version). Its code record is keyed by [`VersionKey`] (function, tier, hooks), so each
//! version is made once and shared by every run.
//!
//! - **The start.** [`Parent::start`] is given what must have code before the program's first
//!   instruction (the entry, and what runs before `main`: the start sections' initializers) and
//!   what the last run's record says ran. The first are wanted at once; [`Effect::Begin`] goes
//!   out when each is published, [`Effect::NoStart`] when one has no code.
//! - **A need.** A thread called a stub whose record the program does not hold. The slot's
//!   wanted version is looked up by key ([`Effect::Lookup`]); found, it is published; absent,
//!   compiled by the [`Compiler`] ([`Effect::Compile`]), then published. However many threads
//!   ask, and however often, one compile of a version is in flight at a time (single flight),
//!   and each asker is answered with the same outcome after the publish.
//! - **No compile waits.** A compile returns its record or the records it lacks
//!   ([`Compiled::Need`]). The version is then blocked and holds no worker; each lacking record
//!   is made once however many compiles lack it ([`Effect::Make`], [`Parent::made`]), and the
//!   compile is entered again from the top once it lacks nothing.
//! - **Speculation.** The callees of each function published, and what the last run ran, are
//!   wanted at lower priorities ([`Priority`]). A background compile never takes the last free
//!   worker, so a thread's own function never waits behind speculation.
//! - **Toggles.** The viewer turns hooks on or off for a function or a set ([`Parent::toggle`]):
//!   the version with the new hooks is found or made at [`Priority::Toggled`] while the slot
//!   keeps running what it ran, then swapped in. With every hook off the slot returns to its
//!   plain version, at the best tier made ([`Parent::tiered`]). A hooked version that does not
//!   compile refuses the toggle ([`Effect::Untoggled`]); the function keeps what it ran.
//! - **Failure (rule 20).** A function whose plain version does not compile fails every caller
//!   with its typed [`CompileError`]; nothing else stops. The run goes on.
//! - **Bounds (rule 18).** A compile, with its re-entries, runs against one deadline on the
//!   driver's clock ([`Config::compile_bound`]); past it, its askers are failed with
//!   [`Why::Overran`]. Waiters are capped ([`Config::most_waiters`]).
//!
//! Mirrors `blaze_lazy::fetch::Fetcher` (the daemon-era single flight, priorities and the worker
//! held back for asks), re-seated: one program, records in place of the daemon's memory, and a
//! lookup before every compile.

#![blaze::sans_io]

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use core::alloc::{Allocator, AllocatorClone};
use core::fmt;

use crate::registry::RunEvent;
use crate::tier::{Identities, Inlined, usable_planted};
use crate::{
    CodeId, CodeRef, CompileError, Hooks, InstanceKey, Made, Nanos, Plant, RecordKey, SlotId,
    ThreadId, Tier, Version, VersionKey, Why,
};

/// What a compile is for, the most urgent first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Priority {
    /// A thread waits for it, or the start does.
    Asked = 0,
    /// The viewer turned a hook on or off for it.
    Toggled = 1,
    /// A function just published calls it.
    Callee = 2,
    /// The last run ran it.
    History = 3,
}

const PRIORITIES: [Priority; 4] = [Priority::Asked, Priority::Toggled, Priority::Callee, Priority::History];

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

/// What the compiler made of one version: its fast-tier code record (written to the store by
/// the compile's task before it ends), the slots its code calls through, and its read list.
pub struct FastRecord<A: Allocator> {
    pub code: CodeRef,
    pub callees: Vec<SlotId, A>,
    pub reads: Vec<Read, A>,
}

/// The fast tier's compiler: the type checker and codegen behind one call (the real one comes
/// from the new item layer, bottom.md). A pure function of what it reads: it returns its record
/// or the records it lacks, and never waits. Its driver calls it for each [`Effect::Compile`].
pub trait Compiler<A: AllocatorClone> {
    fn compile(&mut self, version: VersionKey, alloc: A) -> Result<Made<FastRecord<A>, A>, Why>;
}

/// How one entry of a compile ended, as its driver hands it back.
pub enum Compiled<'r, A: Allocator> {
    Record(&'r FastRecord<A>),
    Need(&'r [RecordKey]),
    Failed(Why),
}

/// What a lookup found in the store: the version's fast record by key, and for the plain
/// version the optimized one by its address, if either is there.
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
    /// Look `key`'s records up, then call [`Parent::looked`].
    Lookup { version: Version, key: VersionKey },
    /// Run the compiler on a worker, then call [`Parent::compiled`]. Past `deadline` the
    /// parent's [`Parent::tick`] fails its askers.
    Compile { version: Version, key: VersionKey, priority: Priority, deadline: Nanos },
    /// Make the record `key` (by the store or by its task), then call [`Parent::made`].
    Make { key: RecordKey, priority: Priority },
    /// Tell the program to place `code` in `slot` (`Slots::publish`).
    Publish { slot: SlotId, tier: Tier, hooks: Hooks, code: CodeRef },
    /// Wake `thread`, which waits on `slot`.
    Answer { thread: ThreadId, slot: SlotId, answer: Answer },
    /// A toggle to `hooks` was refused: that version has no code; the slot keeps what it runs.
    Untoggled { slot: SlotId, hooks: Hooks, error: CompileError },
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
    /// A lookup's answer for a version that was not being looked up.
    NotLooking { version: Version },
    /// A compile's end for a version that was not compiling.
    NotCompiling { version: Version },
    /// A `Need` that names nothing: the compile could never be entered again.
    EmptyNeed { version: Version },
    /// A record made that no compile lacked.
    NotNeeded { key: RecordKey },
    /// Optimized code for a slot whose plain version has no fast code.
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
            Error::NotLooking { version } => write!(
                f,
                "a lookup ended for slot {} hooks {:#x}, which was not being looked up",
                version.slot.0, version.hooks.0
            ),
            Error::NotCompiling { version } => write!(
                f,
                "a compile ended for slot {} hooks {:#x}, which was not compiling",
                version.slot.0, version.hooks.0
            ),
            Error::EmptyNeed { version } => write!(
                f,
                "the compile of slot {} hooks {:#x} lacked nothing and returned no record",
                version.slot.0, version.hooks.0
            ),
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

/// One program's parent. See the module's documentation.
pub struct Parent<A: AllocatorClone> {
    alloc: A,
    config: Config,
    functions: Vec<Function, A>,
    ids: BTreeMap<InstanceKey, CodeId, A>,
    /// Each slot's plain version.
    plain: Vec<State, A>,
    /// Each hooked version asked for; absent is unasked.
    hooked: BTreeMap<Version, State, A>,
    /// The hooks each slot is to run with.
    wanted: Vec<Hooks, A>,
    /// The hooks of what each slot's word names; `None` while it is a stub.
    shown: Vec<Option<Hooks>, A>,
    /// Who waits for which version, in the order they asked.
    waiters: Vec<(Version, Asker), A>,
    queues: [VecDeque<Version, A>; 4],
    /// Each record being made, and the versions whose compiles lack it.
    making: BTreeMap<RecordKey, Vec<Version, A>, A>,
    running: u32,
    running_background: u32,
    start: Start,
    now: Nanos,
    compiles: u64,
    pub(crate) plant: Plant,
}

impl<A: AllocatorClone> Identities for Parent<A> {
    fn code_id(&self, instance: InstanceKey) -> Option<CodeId> {
        self.ids.get(&instance).copied()
    }
}

impl<A: AllocatorClone> Parent<A> {
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
        let mut all = Vec::with_capacity_in(n, alloc.clone());
        all.extend_from_slice(functions);
        let mut ids = BTreeMap::new_in(alloc.clone());
        for function in functions {
            ids.insert(function.instance, function.code);
        }
        let mut plain = Vec::with_capacity_in(n, alloc.clone());
        plain.resize(n, State::Unasked);
        let mut wanted = Vec::with_capacity_in(n, alloc.clone());
        wanted.resize(n, Hooks::NONE);
        let mut shown = Vec::with_capacity_in(n, alloc.clone());
        shown.resize(n, None);
        Ok(Parent {
            config,
            functions: all,
            ids,
            plain,
            hooked: BTreeMap::new_in(alloc.clone()),
            wanted,
            shown,
            waiters: Vec::new_in(alloc.clone()),
            queues: [
                VecDeque::new_in(alloc.clone()),
                VecDeque::new_in(alloc.clone()),
                VecDeque::new_in(alloc.clone()),
                VecDeque::new_in(alloc.clone()),
            ],
            making: BTreeMap::new_in(alloc.clone()),
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
        self.functions.len() as u32
    }

    fn check(&self, slot: SlotId) -> Result<(), Error> {
        match slot.index() < self.functions.len() {
            true => Ok(()),
            false => Err(Error::NoSlot { slot, slots: self.slots() }),
        }
    }

    /// Where a hooked version's state is kept: by the version (the planted bug forgets the slot).
    fn home(&self, version: Version) -> Version {
        match self.plant {
            Plant::VersionsByHooks => Version { slot: SlotId(0), hooks: version.hooks },
            _ => version,
        }
    }

    fn state(&self, version: Version) -> State {
        match version.hooks.is_none() {
            true => self.plain[version.slot.index()],
            false => self.hooked.get(&self.home(version)).copied().unwrap_or(State::Unasked),
        }
    }

    fn set(&mut self, version: Version, state: State) {
        match version.hooks.is_none() {
            true => self.plain[version.slot.index()] = state,
            false => {
                let home = self.home(version);
                self.hooked.insert(home, state);
            }
        }
    }

    fn key(&self, version: Version) -> VersionKey {
        VersionKey {
            function: self.functions[version.slot.index()].instance,
            tier: Tier::Fast,
            hooks: version.hooks,
        }
    }

    /// The version `slot` is to run now.
    fn current(&self, slot: SlotId) -> Version {
        Version { slot, hooks: self.wanted[slot.index()] }
    }

    pub fn function(&self, slot: SlotId) -> Option<Function> {
        self.functions.get(slot.index()).copied()
    }

    /// How many compile entries were started (re-entries after a `Need` included).
    pub fn compiles(&self) -> u64 {
        self.compiles
    }

    /// Whether `slot`'s plain version has code, and at which tier.
    pub fn ready(&self, slot: SlotId) -> Option<Tier> {
        match self.plain.get(slot.index()) {
            Some(State::Ready { tier, .. }) => Some(*tier),
            _ => None,
        }
    }

    /// The hooks `slot` is to run with, and those of what its word names now.
    pub fn hooks(&self, slot: SlotId) -> Option<(Hooks, Option<Hooks>)> {
        Some((*self.wanted.get(slot.index())?, self.shown[slot.index()]))
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
            let version = self.current(*slot);
            if self.waiters.iter().any(|it| *it == (version, Asker::Start)) {
                continue;
            }
            match self.state(version) {
                State::Ready { .. } => continue,
                State::Failed { error, .. } => {
                    self.refuse_start(error, out);
                    return Ok(());
                }
                _ => {}
            }
            self.wait(version, Asker::Start)?;
            if let Start::Waiting { left } = &mut self.start {
                *left += 1;
            }
            self.want(version, Priority::Asked, out);
        }
        for slot in history {
            self.want(self.current(*slot), Priority::History, out);
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
        let version = self.current(slot);
        match self.state(version) {
            State::Ready { tier, .. } => {
                out.push(Effect::Answer { thread, slot, answer: Answer::Code { tier } });
            }
            State::Failed { error, .. } => {
                out.push(Effect::Answer { thread, slot, answer: Answer::Failed(error) });
            }
            State::Compiling { priority, deadline } if self.plant == Plant::CompileTwice => {
                self.wait(version, Asker::Thread(thread))?;
                let key = self.key(version);
                out.push(Effect::Compile { version, key, priority, deadline });
            }
            State::Unasked
            | State::Looking { .. }
            | State::Queued { .. }
            | State::Compiling { .. }
            | State::Blocked { .. } => {
                self.wait(version, Asker::Thread(thread))?;
                self.want(version, Priority::Asked, out);
            }
        }
        self.pump(out);
        Ok(())
    }

    /// The viewer turned `hooks` on or off for each of `slots`: each slot's new version is found
    /// or made, then swapped in; the slot runs what it ran until then.
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
            let was = self.wanted[slot.index()];
            let now = was.with(hooks, on);
            if now == was {
                continue;
            }
            if now.is_none() && self.plant == Plant::OffKeepsHooks {
                continue;
            }
            self.wanted[slot.index()] = now;
            // A function never called has nothing to swap: its first need asks for this version.
            let Some(shown) = self.shown[slot.index()] else { continue };
            if shown == now {
                continue;
            }
            let version = Version { slot: *slot, hooks: now };
            match self.state(version) {
                State::Ready { tier, code } => self.show(version, tier, code, out),
                State::Failed { error, .. } => self.untoggle(version, error, out),
                _ => self.want(version, Priority::Toggled, out),
            }
        }
        self.pump(out);
        Ok(())
    }

    /// The tier made optimized `code` for `slot`'s plain version: its best plain code from now,
    /// shown at once unless the slot runs with hooks.
    pub fn tiered(&mut self, slot: SlotId, code: CodeRef, out: &mut Vec<Effect, A>) -> Result<(), Error> {
        self.check(slot)?;
        let version = Version { slot, hooks: Hooks::NONE };
        let State::Ready { .. } = self.plain[slot.index()] else {
            return Err(Error::NotReady { slot });
        };
        self.plain[slot.index()] = State::Ready { tier: Tier::Optimized, code };
        if self.wanted[slot.index()].is_none() {
            self.show(version, Tier::Optimized, code, out);
        }
        Ok(())
    }

    fn wait(&mut self, version: Version, asker: Asker) -> Result<(), Error> {
        if self.waiters.contains(&(version, asker)) {
            return Ok(());
        }
        if self.waiters.len() >= self.config.most_waiters as usize {
            return Err(Error::TooManyWaiters { most: self.config.most_waiters, slot: version.slot });
        }
        self.waiters.push((version, asker));
        Ok(())
    }

    /// Wants `version` at `priority`: looked up first, or moved up in its queue.
    fn want(&mut self, version: Version, priority: Priority, out: &mut Vec<Effect, A>) {
        match self.state(version) {
            State::Unasked => {
                self.set(version, State::Looking { priority });
                let key = self.key(version);
                out.push(Effect::Lookup { version, key });
            }
            State::Looking { priority: was } if priority < was => {
                self.set(version, State::Looking { priority });
            }
            State::Queued { priority: was, deadline } if priority < was => {
                self.set(version, State::Queued { priority, deadline });
                self.queues[priority as usize].push_back(version);
            }
            State::Looking { .. }
            | State::Queued { .. }
            | State::Compiling { .. }
            | State::Blocked { .. }
            | State::Ready { .. }
            | State::Failed { .. } => {}
        }
    }

    /// The store's answer to [`Effect::Lookup`] for `version`.
    pub fn looked(
        &mut self,
        version: Version,
        found: Option<Found<'_>>,
        out: &mut Vec<Effect, A>,
    ) -> Result<(), Error> {
        self.check(version.slot)?;
        let State::Looking { priority } = self.state(version) else {
            return Err(Error::NotLooking { version });
        };
        match found {
            Some(found) => {
                let optimized = found.optimized.filter(|it| {
                    version.hooks.is_none() && usable_planted(it.inlined, &*self, self.plant).is_ok()
                });
                let (tier, code) = match optimized {
                    Some(optimized) => (Tier::Optimized, optimized.code),
                    None => (Tier::Fast, found.fast),
                };
                self.ready(version, tier, code, found.callees, out);
            }
            None => {
                self.set(version, State::Queued { priority, deadline: None });
                self.queues[priority as usize].push_back(version);
            }
        }
        self.pump(out);
        Ok(())
    }

    /// The end of one entry of [`Effect::Compile`] for `version`.
    pub fn compiled(
        &mut self,
        version: Version,
        ended: Compiled<'_, A>,
        now: Nanos,
        out: &mut Vec<Effect, A>,
    ) -> Result<(), Error> {
        self.check(version.slot)?;
        self.now = self.now.max(now);
        let (priority, deadline, late) = match self.state(version) {
            State::Compiling { priority, deadline } => (priority, deadline, false),
            State::Failed { compiling: Some(priority), error } => {
                self.set(version, State::Failed { error, compiling: None });
                (priority, self.now, true)
            }
            _ => return Err(Error::NotCompiling { version }),
        };
        self.running -= 1;
        if priority != Priority::Asked {
            self.running_background -= 1;
        }
        match ended {
            // A late success still serves every later caller; the early ones were failed.
            Compiled::Record(record) => {
                self.ready(version, Tier::Fast, record.code, &record.callees, out);
            }
            Compiled::Need(_) | Compiled::Failed(_) if late => {}
            Compiled::Need(keys) => {
                if keys.is_empty() {
                    return Err(Error::EmptyNeed { version });
                }
                let mut left = 0;
                for key in keys {
                    match self.making.get_mut(key) {
                        Some(versions) if versions.contains(&version) => continue,
                        Some(versions) => versions.push(version),
                        None => {
                            let mut versions = Vec::new_in(self.alloc.clone());
                            versions.push(version);
                            self.making.insert(*key, versions);
                            out.push(Effect::Make { key: *key, priority });
                        }
                    }
                    left += 1;
                }
                self.set(version, State::Blocked { priority, deadline, left });
            }
            Compiled::Failed(why) => {
                let error = CompileError { function: self.functions[version.slot.index()].instance, slot: version.slot, why };
                self.fail(version, error, out);
            }
        }
        self.pump(out);
        Ok(())
    }

    /// The record `key` was made (or could not be): each compile that lacked it is entered again
    /// once it lacks nothing, or fails with the record's error.
    pub fn made(&mut self, key: RecordKey, result: Result<(), Why>, out: &mut Vec<Effect, A>) -> Result<(), Error> {
        let Some(versions) = self.making.remove(&key) else {
            return Err(Error::NotNeeded { key });
        };
        for version in versions {
            let State::Blocked { priority, deadline, left } = self.state(version) else { continue };
            match result {
                Err(why) => {
                    let function = self.functions[version.slot.index()].instance;
                    self.fail(version, CompileError { function, slot: version.slot, why }, out);
                }
                Ok(()) if left > 1 && self.plant != Plant::ReenterEarly => {
                    self.set(version, State::Blocked { priority, deadline, left: left - 1 });
                }
                Ok(()) => {
                    self.set(version, State::Queued { priority, deadline: Some(deadline) });
                    self.queues[priority as usize].push_front(version);
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
        let each = (0..self.plain.len()).map(|at| Version { slot: SlotId(at as u32), hooks: Hooks::NONE });
        for version in each.chain(self.hooked.keys().copied()) {
            let (deadline, holds) = match self.state(version) {
                State::Compiling { priority, deadline } => (deadline, Some(priority)),
                State::Blocked { deadline, .. } => (deadline, None),
                State::Queued { deadline: Some(deadline), .. } => (deadline, None),
                _ => continue,
            };
            if deadline <= self.now {
                over.push((version, holds));
            }
        }
        for (version, holds) in over {
            let why = Why::Overran { bound: self.config.compile_bound };
            let function = self.functions[version.slot.index()].instance;
            let error = CompileError { function, slot: version.slot, why };
            self.fail(version, error, out);
            self.set(version, State::Failed { error, compiling: holds });
        }
        self.pump(out);
    }

    /// `version` has code: kept, shown if it is what its slot is to run, its askers answered,
    /// its callees wanted.
    fn ready(&mut self, version: Version, tier: Tier, code: CodeRef, callees: &[SlotId], out: &mut Vec<Effect, A>) {
        self.set(version, State::Ready { tier, code });
        let function = self.functions[version.slot.index()].instance;
        out.push(Effect::Say(match tier {
            Tier::Fast => RunEvent::Published { function },
            Tier::Optimized => RunEvent::Tiered { function },
        }));
        if self.current(version.slot) == version {
            self.show(version, tier, code, out);
        }
        self.answer(version, Answer::Code { tier }, out);
        for callee in callees {
            if callee.index() < self.functions.len() {
                self.want(self.current(*callee), Priority::Callee, out);
            }
        }
    }

    /// Swaps `version`'s code into its slot's word.
    fn show(&mut self, version: Version, tier: Tier, code: CodeRef, out: &mut Vec<Effect, A>) {
        let slot = version.slot;
        self.shown[slot.index()] = Some(version.hooks);
        out.push(Effect::Publish { slot, tier, hooks: version.hooks, code });
    }

    fn fail(&mut self, version: Version, error: CompileError, out: &mut Vec<Effect, A>) {
        self.set(version, State::Failed { error, compiling: None });
        out.push(Effect::Say(RunEvent::Failed { function: error.function }));
        if !version.hooks.is_none() {
            self.untoggle(version, error, out);
            return;
        }
        self.answer(version, Answer::Failed(error), out);
        if self.plant == Plant::StopOnFailure {
            out.push(Effect::Stop(error));
        }
    }

    /// A hooked version has no code: the toggle is refused, the slot is to run what it shows (or
    /// its plain version), and anyone waiting for the hooked one waits for that instead.
    fn untoggle(&mut self, version: Version, error: CompileError, out: &mut Vec<Effect, A>) {
        let slot = version.slot;
        if self.wanted[slot.index()] == version.hooks {
            self.wanted[slot.index()] = self.shown[slot.index()].unwrap_or(Hooks::NONE);
        }
        out.push(Effect::Untoggled { slot, hooks: version.hooks, error });
        let instead = self.current(slot);
        let mut moved = false;
        for (waits, _) in &mut self.waiters {
            if *waits == version {
                *waits = instead;
                moved = true;
            }
        }
        if !moved {
            return;
        }
        match self.state(instead) {
            State::Ready { tier, .. } => self.answer(instead, Answer::Code { tier }, out),
            State::Failed { error, .. } => self.answer(instead, Answer::Failed(error), out),
            _ => self.want(instead, Priority::Asked, out),
        }
    }

    /// Answers `version`'s waiters in the order they asked, and the start if it waited.
    fn answer(&mut self, version: Version, answer: Answer, out: &mut Vec<Effect, A>) {
        let mut start = false;
        let slot = version.slot;
        self.waiters.retain(|(waits, asker)| {
            if *waits != version {
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
                let Some(version) = self.queues[priority as usize].pop_front() else { break };
                let State::Queued { priority: at, deadline } = self.state(version) else { continue };
                if at != priority {
                    continue;
                }
                let deadline = deadline.unwrap_or(self.now.after(self.config.compile_bound));
                self.set(version, State::Compiling { priority, deadline });
                self.running += 1;
                self.running_background += u32::from(background);
                self.compiles += 1;
                let key = self.key(version);
                out.push(Effect::Compile { version, key, priority, deadline });
                out.push(Effect::Say(RunEvent::Compiling { function: key.function }));
            }
        }
    }
}
