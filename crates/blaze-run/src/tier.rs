//! [`Tier`]: the optimizing tier. It takes the functions [`crate::heat::Heat`] found hot, asks
//! the [`Optimizer`] for each once, at the lowest priority of the parent's work, and has the
//! result filed and published.
//!
//! **The record.** An optimized function is its own record, never a replacement of the fast one:
//! keyed by its instance at [`crate::Tier::Optimized`], and valid only while every callee body it
//! absorbed has the code identity it had ([`Optimized::inlined`], transitively: a callee's callee
//! inlined through it is listed too). It is filed read-addressed (blaze-keyed's `Trace::Read`):
//! the list is its shape, the identities its digests, so a lookup with today's identities finds
//! it only if it still holds, and [`usable`] is that check as the parent makes it on load. A stale
//! optimized record costs nothing: the fast record is found under its own key and runs.
//!
//! **No waiting.** The optimizer returns its record or the records it lacks ([`Made::Need`]); the
//! job is then blocked, the driver makes each lacking record once however many jobs lack it
//! ([`Effect::Make`], [`Tier::made`]), and the job is entered again from the top once all are
//! made. A job holds a worker only while it runs.
//!
//! **Snapshots.** A job is made against the snapshot the tier was at when it began. The parent
//! tells the tier of a newer one ([`Tier::newer`]) when another run of the same checkout began on
//! newer text; from then a job of the older snapshot that ends, or is unblocked, is dropped,
//! never filed or published, and nothing queued before it starts.
//!
//! **The reverse map.** [`Tier::callers_of`] gives, for a callee's code identity, the functions
//! whose optimized code absorbed it: what a later edit of that callee takes back to fast code
//! (run.md, "Open questions": live patching), and what a run's start need not look up.

#![blaze::sans_io]

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use core::alloc::{Allocator, AllocatorClone};
use core::fmt;

use crate::{CodeId, CodeRef, CompileError, InstanceKey, Made, Plant, RecordKey, SlotId, Snapshot, Why};

/// One callee body an optimized record absorbed, with the code identity it had then.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Inlined {
    pub instance: InstanceKey,
    pub code: CodeId,
}

/// What the optimizer made of one function: its code record, and every callee body in it.
pub struct Optimized<A: Allocator> {
    pub code: CodeRef,
    pub inlined: Vec<Inlined, A>,
}

/// A callee the optimizer may inline: its instance, whose body it reads by key, and that body's
/// code identity now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Callee {
    pub instance: InstanceKey,
    pub code: CodeId,
}

/// The optimizing backend (the real one comes from codegen's top tier, function grain). A pure
/// function of what it reads: it returns its record, or what it lacks, and never waits.
pub trait Optimizer<A: AllocatorClone> {
    /// `instance` optimized, with any of `callees` (and, through them, their callees) inlined.
    /// The result lists every body absorbed with the identity it read; a body missing from the
    /// list is a stale-code bug of the optimizer's.
    fn optimize(
        &mut self,
        instance: InstanceKey,
        callees: &[Callee],
        alloc: A,
    ) -> Result<Made<Optimized<A>, A>, Why>;
}

/// The code identity each instance has now: the program's manifest, as the parent holds it.
pub trait Identities {
    fn code_id(&self, instance: InstanceKey) -> Option<CodeId>;
}

/// Why an optimized record no longer holds: the first callee whose body changed (or is gone).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stale {
    pub callee: InstanceKey,
    pub was: CodeId,
    pub now: Option<CodeId>,
}

/// Whether a record that absorbed `inlined` holds against today's identities.
pub fn usable(inlined: &[Inlined], now: &impl Identities) -> Result<(), Stale> {
    usable_planted(inlined, now, Plant::None)
}

pub(crate) fn usable_planted(
    inlined: &[Inlined],
    now: &impl Identities,
    plant: Plant,
) -> Result<(), Stale> {
    if plant == Plant::TrustOptimized {
        return Ok(());
    }
    for each in inlined {
        let current = now.code_id(each.instance);
        if current != Some(each.code) {
            return Err(Stale { callee: each.instance, was: each.code, now: current });
        }
    }
    Ok(())
}

/// One optimization to run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Job {
    pub slot: SlotId,
    pub instance: InstanceKey,
    pub snapshot: Snapshot,
}

/// How one entry of a job ended, as its driver hands it back.
pub enum Ended<'r, A: Allocator> {
    Record(Optimized<A>),
    Need(&'r [RecordKey]),
    Failed(Why),
}

/// What the tier asks its driver to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Effect {
    /// Run the optimizer for `job` at the lowest priority, then call [`Tier::done`].
    Optimize(Job),
    /// Make the record `key` (by the store or by its task), then call [`Tier::made`].
    Make { key: RecordKey },
    /// Write [`Tier::record`]`(slot)` to the store, read-addressed by its inlined list.
    File { slot: SlotId },
    /// Tell the parent `slot` has optimized `code` ([`crate::parent::Parent::tiered`]).
    Publish { slot: SlotId, code: CodeRef },
    /// The optimizer failed where the fast tier did not: blaze's bug, reported; the fast code runs.
    Failed { slot: SlotId, error: CompileError },
    /// A job of an older snapshot ended and was dropped.
    Dropped { slot: SlotId, snapshot: Snapshot },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    NoSlot { slot: SlotId, slots: u32 },
    /// A slot given to the tier again: heat gives each once.
    Twice { slot: SlotId },
    /// An end of a job the tier is not running.
    NotRunning { slot: SlotId, snapshot: Snapshot },
    /// A snapshot that is not newer than the tier's.
    NotNewer { asked: Snapshot, at: Snapshot },
    /// A `Need` that names nothing: the job could never be entered again.
    EmptyNeed { slot: SlotId },
    /// A record made that no job lacked.
    NotNeeded { key: RecordKey },
    Config { what: &'static str },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::NoSlot { slot, slots } => {
                write!(f, "the tier was given slot {}, past the table's {slots}", slot.0)
            }
            Error::Twice { slot } => {
                write!(f, "slot {} was given to the tier twice: tiered twice", slot.0)
            }
            Error::NotRunning { slot, snapshot } => write!(
                f,
                "an optimization of slot {} at snapshot {} ended, and none was running",
                slot.0, snapshot.0
            ),
            Error::NotNewer { asked, at } => {
                write!(f, "snapshot {} is not newer than the tier's {}", asked.0, at.0)
            }
            Error::EmptyNeed { slot } => {
                write!(f, "the optimization of slot {} lacked nothing and returned no record", slot.0)
            }
            Error::NotNeeded { key } => {
                write!(f, "record {:02x?} was made and no optimization lacked it", &key.0[..8])
            }
            Error::Config { what } => write!(f, "the tier's config: {what}"),
        }
    }
}

impl core::error::Error for Error {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Fast,
    Queued { snapshot: Snapshot },
    Optimizing { snapshot: Snapshot },
    /// Waits for `left` records to be made; holds no worker.
    Blocked { snapshot: Snapshot, left: u32 },
    Optimized,
    Failed,
    Dropped,
}

/// The optimizing tier of one run. See the module's documentation.
pub struct Tier<A: AllocatorClone> {
    alloc: A,
    state: Vec<State, A>,
    instances: BTreeMap<SlotId, InstanceKey, A>,
    queue: VecDeque<SlotId, A>,
    workers: u32,
    running: u32,
    snapshot: Snapshot,
    records: BTreeMap<SlotId, Optimized<A>, A>,
    inlined_into: BTreeMap<CodeId, Vec<SlotId, A>, A>,
    /// Each record being made, and the jobs that lack it.
    making: BTreeMap<RecordKey, Vec<SlotId, A>, A>,
}

impl<A: AllocatorClone> Tier<A> {
    /// A tier of `slots` functions running at most `workers` optimizations at once.
    pub fn new(slots: u32, workers: u32, snapshot: Snapshot, alloc: A) -> Result<Tier<A>, Error> {
        if workers == 0 {
            return Err(Error::Config { what: "no worker" });
        }
        let mut state = Vec::new_in(alloc.clone());
        state.resize(slots as usize, State::Fast);
        Ok(Tier {
            state,
            instances: BTreeMap::new_in(alloc.clone()),
            queue: VecDeque::new_in(alloc.clone()),
            workers,
            running: 0,
            snapshot,
            records: BTreeMap::new_in(alloc.clone()),
            inlined_into: BTreeMap::new_in(alloc.clone()),
            making: BTreeMap::new_in(alloc.clone()),
            alloc,
        })
    }

    /// `slot`, which runs `instance`, is hot: queued, and started if a worker is free.
    pub fn hot(
        &mut self,
        slot: SlotId,
        instance: InstanceKey,
        out: &mut Vec<Effect, A>,
    ) -> Result<(), Error> {
        let slots = self.state.len() as u32;
        let snapshot = self.snapshot;
        let Some(state) = self.state.get_mut(slot.index()) else {
            return Err(Error::NoSlot { slot, slots });
        };
        if *state != State::Fast {
            return Err(Error::Twice { slot });
        }
        *state = State::Queued { snapshot };
        self.instances.insert(slot, instance);
        self.queue.push_back(slot);
        self.pump(out);
        Ok(())
    }

    fn pump(&mut self, out: &mut Vec<Effect, A>) {
        while self.running < self.workers {
            let Some(slot) = self.queue.pop_front() else { return };
            let State::Queued { snapshot } = self.state[slot.index()] else { continue };
            if snapshot < self.snapshot {
                self.state[slot.index()] = State::Dropped;
                out.push(Effect::Dropped { slot, snapshot });
                continue;
            }
            self.state[slot.index()] = State::Optimizing { snapshot };
            self.running += 1;
            let instance = self.instances[&slot];
            out.push(Effect::Optimize(Job { slot, instance, snapshot }));
        }
    }

    /// The end of one entry of `job`.
    pub fn done(&mut self, job: Job, ended: Ended<'_, A>, out: &mut Vec<Effect, A>) -> Result<(), Error> {
        let Job { slot, snapshot, instance } = job;
        if self.state.get(slot.index()) != Some(&State::Optimizing { snapshot }) {
            return Err(Error::NotRunning { slot, snapshot });
        }
        self.running -= 1;
        if snapshot < self.snapshot {
            self.state[slot.index()] = State::Dropped;
            out.push(Effect::Dropped { slot, snapshot });
            self.pump(out);
            return Ok(());
        }
        match ended {
            Ended::Record(optimized) => {
                for each in &optimized.inlined {
                    let callers = self
                        .inlined_into
                        .entry(each.code)
                        .or_insert_with(|| Vec::new_in(self.alloc.clone()));
                    if !callers.contains(&slot) {
                        callers.push(slot);
                    }
                }
                let code = optimized.code;
                self.records.insert(slot, optimized);
                self.state[slot.index()] = State::Optimized;
                out.push(Effect::File { slot });
                out.push(Effect::Publish { slot, code });
            }
            Ended::Need(keys) => {
                if keys.is_empty() {
                    self.state[slot.index()] = State::Failed;
                    return Err(Error::EmptyNeed { slot });
                }
                let mut left = 0;
                for key in keys {
                    match self.making.get_mut(key) {
                        Some(jobs) if jobs.contains(&slot) => continue,
                        Some(jobs) => jobs.push(slot),
                        None => {
                            let mut jobs = Vec::new_in(self.alloc.clone());
                            jobs.push(slot);
                            self.making.insert(*key, jobs);
                            out.push(Effect::Make { key: *key });
                        }
                    }
                    left += 1;
                }
                self.state[slot.index()] = State::Blocked { snapshot, left };
            }
            Ended::Failed(why) => {
                self.state[slot.index()] = State::Failed;
                let error = CompileError { function: instance, slot, why };
                out.push(Effect::Failed { slot, error });
            }
        }
        self.pump(out);
        Ok(())
    }

    /// The record `key` was made (or could not be): each job that lacked it is entered again
    /// once it lacks nothing, or fails.
    pub fn made(&mut self, key: RecordKey, result: Result<(), Why>, out: &mut Vec<Effect, A>) -> Result<(), Error> {
        let Some(jobs) = self.making.remove(&key) else {
            return Err(Error::NotNeeded { key });
        };
        for slot in jobs {
            let State::Blocked { snapshot, left } = self.state[slot.index()] else { continue };
            match result {
                Err(why) => {
                    self.state[slot.index()] = State::Failed;
                    let error = CompileError { function: self.instances[&slot], slot, why };
                    out.push(Effect::Failed { slot, error });
                }
                Ok(()) if left > 1 => {
                    self.state[slot.index()] = State::Blocked { snapshot, left: left - 1 };
                }
                Ok(()) => {
                    self.state[slot.index()] = State::Queued { snapshot };
                    self.queue.push_front(slot);
                }
            }
        }
        self.pump(out);
        Ok(())
    }

    /// A newer snapshot exists: what runs or is blocked now is dropped when it ends or is
    /// unblocked, what is queued now never starts.
    pub fn newer(&mut self, snapshot: Snapshot) -> Result<(), Error> {
        if snapshot <= self.snapshot {
            return Err(Error::NotNewer { asked: snapshot, at: self.snapshot });
        }
        self.snapshot = snapshot;
        for slot in self.queue.drain(..) {
            if let State::Queued { .. } = self.state[slot.index()] {
                self.state[slot.index()] = State::Dropped;
            }
        }
        Ok(())
    }

    /// The optimized record of `slot`, once made: what [`Effect::File`] writes.
    pub fn record(&self, slot: SlotId) -> Option<&Optimized<A>> {
        self.records.get(&slot)
    }

    /// The functions whose optimized code absorbed the body with identity `code`.
    pub fn callers_of(&self, code: CodeId) -> &[SlotId] {
        self.inlined_into.get(&code).map_or(&[], |callers| callers.as_slice())
    }

    pub fn running(&self) -> u32 {
        self.running
    }
}
