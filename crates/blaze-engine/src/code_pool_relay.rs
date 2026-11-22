//! The code relay of D194: making code never happens on the command thread. Each child (one
//! test's own process, by its runner row) asks for at most one instance at a time (its lazy
//! parent stops at the waiting frame, blaze-cli run_parent.rs `Lazy::asked`); the relay drives
//! the ONE [`CodePool`] (rule 30: no second table) so that an instance is made once while in
//! flight however many children ask it, by a worker of the command's pool, and the command
//! thread only relays: it hands each [`Relay::job`] to its worker, gives each result back
//! ([`Relay::made`], [`Relay::refused`]) and takes each child's answer ([`Relay::answer`]).
//!
//! What the pool is driven with here, and why:
//!
//! - **No speculation**: frontier 0 and no direct calls. A child's walk declares what a record
//!   calls; the instances are made when the program first calls them (run.md D124), never ahead.
//! - **No store here**: a child's walk looked in the home's store before it asked (blaze-ladder
//!   body_ids.rs `Walk::begin` answers a found entry itself), so every key the pool looks up is
//!   answered "absent" at once, and the made entry is put by the child's own walk
//!   (`Walk::finish`) on the command thread, where the store lives.
//! - **Unkeyed asks are their own**: an instance the walk cannot key (body_ids.rs `Unkeyed`) gets
//!   a key of its own, never another ask's, so it is made for its child alone.
//! - **One thread a child**: every ask is `Thread(0)`; the pool's per-child order (code_pool.rs
//!   `ChildState::pending`) means one child's long make never holds another child's answer.
//!
//! Bounded (rules 15, 18): every ask has its own deadline, [`Options::code_wait`] after it was
//! asked ([`Wait::Code`]); past it the ask is answered [`Reply::Late`] naming the key and the
//! child is withdrawn from the pool, so nothing is published to it after. The pool keeps the
//! request's deadline for the makes themselves. The bytes kept for later askers are capped
//! ([`Options::most_bytes`]); past the cap the make is refused by name ([`RelayError::Full`]).
//!
//! A made instance's bytes are kept, keyed, until the relay ends: a child that asks a key the
//! pool already made is answered at once from them (its walk may ask before the first asker's
//! walk has put the entry in the store). An unkeyed instance's bytes go when its child takes them.
//! A refusal is kept, keyed, as made bytes are, and EVERY asker of its key is handed the same
//! refusal ([`Reply::Refused`], a borrow of the one kept): each test whose instance could not be
//! made prints the cause itself, never a pointer to another test (rule 19; tempo t2000p: 277 NOT
//! RUN lines said only "the cause is said with the test that asked it first"). A refusal's size
//! is its driver's (blaze-cli test_drive.rs `CodeRefusal`, its words capped there); there is one
//! a key, and the keys are bounded by [`Options::asks`].

#![blaze::sans_io]

use blaze_id::ContentId;
use blaze_own::{SharedAlloc, VecDeque};

use super::{Action, Cause, Child, CodeKey, CodePool, Error as PoolError, Event, Finished, Options as PoolOptions, Record, Thread, Time, Wait as PoolWait, Worker};
use crate::{SMap, SSet, SVec};

/// The domain of an unkeyed ask's own key (a counter under it): never a walk's key.
const UNKEYED: &str = "blaze code relay unkeyed ask 1";

#[derive(Clone, Copy)]
pub struct Options<'a> {
    pub alloc: SharedAlloc<'a>,
    pub hash_seed: u64,
    /// Children have dense ids from zero: the runner's rows.
    pub children: usize,
    /// Workers have dense ids from zero, each fed through its own queue by the driver.
    pub workers: usize,
    /// The most asks over the relay's life (the pool's demand budget).
    pub asks: usize,
    /// The request's absolute deadline, on the driver's clock.
    pub deadline: Time,
    /// How long one ask may wait for its code, in the clock's units.
    pub code_wait: u64,
    /// The most bytes of made entries kept at once.
    pub most_bytes: usize,
}

/// What the relay waits on, for the driver's named wait (rule 15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// Child `child`'s ask of `key`, by the ask's own deadline.
    Code { child: Child, key: CodeKey },
    /// The pool's wait (a worker for a key, a make), by the request's deadline.
    Pool(PoolWait),
}

/// A child's answer, taken once ([`Relay::answer`]).
#[derive(Debug)]
pub enum Reply<'r, E> {
    /// The made entry's bytes: the child's walk finishes its pending row with them.
    Made(&'r [u8]),
    /// The worker's refusal of the make, the same one to every asker of its key.
    Refused(&'r E),
    /// The ask of `key` waited `waited` (the clock's units) and its code did not come.
    Late { key: CodeKey, waited: u64 },
}

/// Why the relay stopped: each names the child, worker or key (rule 19). Every one is a fault of
/// the driver or a bound, never of one test's code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelayError {
    /// The pool refused a step (its own words: a deadline, the demand budget).
    Pool(PoolError),
    UnknownChild(Child),
    /// The child ended ([`Relay::exited`]) and asked, or ended again.
    ExitedChild(Child),
    /// The child asked while its ask of `asked` is unanswered or its answer not taken.
    TwoAsks { child: Child, asked: CodeKey },
    /// An answer of `key` for `child`, which is not asking it (or ended).
    NotAsking { child: Child, key: CodeKey },
    /// A result from `worker`, which was given no make.
    NotOwner { worker: Worker },
    /// The pool built `key` and no ask left its order.
    NoOrder(CodeKey),
    /// The pool sent `key` to a producer: a relay's worker never waits on one.
    Producer(CodeKey),
    /// The pool answered `key` and the relay holds nothing made for it.
    NotMade(CodeKey),
    /// Keeping `key`'s `bytes` would pass the cap of `most` kept bytes.
    Full { key: CodeKey, bytes: usize, most: usize },
}

impl core::fmt::Display for RelayError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let rule = "the code relay (code_pool_relay.rs, D194)";
        match self {
            Self::Pool(why) => write!(f, "{rule}: {why}"),
            Self::UnknownChild(child) => write!(f, "{rule}: child {} is outside the relay", child.0),
            Self::ExitedChild(child) => write!(f, "{rule}: child {} ended and was told again", child.0),
            Self::TwoAsks { child, asked } => write!(f, "{rule}: child {} asked again while its ask of {asked} is open", child.0),
            Self::NotAsking { child, key } => write!(f, "{rule}: an answer of {key} for child {}, which is not asking it (an answer to an ended child is refused)", child.0),
            Self::NotOwner { worker } => write!(f, "{rule}: a result from worker {}, which was given no make", worker.0),
            Self::NoOrder(key) => write!(f, "{rule}: {key} was to be made and no ask left its order"),
            Self::Producer(key) => write!(f, "{rule}: {key} was sent to a producer, and a relay's maker never waits on one"),
            Self::NotMade(key) => write!(f, "{rule}: {key} was answered and nothing made is held for it"),
            Self::Full { key, bytes, most } => write!(f, "{rule}: keeping the {bytes} bytes made for {key} passes the cap of {most} kept bytes (rule 18)"),
        }
    }
}

impl core::error::Error for RelayError {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Live,
    /// Withdrawn from the pool by its ask's deadline; the driver still ends it.
    Late,
    Exited,
}

#[derive(Clone, Copy)]
struct Asked {
    key: CodeKey,
    due: u64,
}

/// A key's order until the pool gives it a worker, and whether the key is an unkeyed ask's own.
struct Ordered<J> {
    order: J,
    unkeyed: bool,
}

#[derive(Clone, Copy)]
enum Got {
    Made(CodeKey),
    Refused(CodeKey),
    Late { key: CodeKey, waited: u64 },
}

struct Asker {
    end: End,
    asked: Option<Asked>,
    got: Option<Got>,
}

struct Entry<'a, E> {
    /// `None`: the make was refused.
    bytes: Option<SVec<'a, u8>>,
    /// The make's refusal, handed to every asker (never taken).
    refusal: Option<E>,
    unkeyed: bool,
    /// Answers set for this key (the planted dropped wake skips all but the first).
    #[cfg(test)]
    answered: usize,
}

pub struct Relay<'a, J, E> {
    opts: Options<'a>,
    pool: CodePool<'a>,
    askers: SVec<'a, Asker>,
    /// Each ask's deadline, earliest first: (due, child).
    due: SSet<'a, (u64, usize)>,
    /// The order of each key the pool has not given a worker yet (the first asker's).
    orders: SMap<'a, CodeKey, Ordered<J>>,
    /// The key each worker makes, and whether it is an unkeyed ask's own.
    building: SVec<'a, Option<(CodeKey, bool)>>,
    jobs: VecDeque<(Worker, CodeKey, J), SharedAlloc<'a>>,
    entries: SMap<'a, CodeKey, Entry<'a, E>>,
    held: usize,
    unkeyed: u64,
    actions: SVec<'a, Action>,
    #[cfg(test)]
    plant: Plant,
}

impl<'a, J, E> Relay<'a, J, E> {
    pub fn new(now: Time, opts: Options<'a>) -> Result<Self, RelayError> {
        let a = opts.alloc;
        let pool = CodePool::new(PoolOptions {
            alloc: a, hash_seed: opts.hash_seed, children: opts.children, workers: opts.workers, frontier: 0, demands: opts.asks, deadline: opts.deadline,
        })
        .map_err(RelayError::Pool)?;
        let mut askers = SVec::with_capacity_in(opts.children, a);
        askers.resize_with(opts.children, || Asker { end: End::Live, asked: None, got: None });
        let mut building = SVec::with_capacity_in(opts.workers, a);
        building.resize(opts.workers, None);
        let mut relay = Relay {
            opts, pool, askers, due: SSet::new_in(a), orders: SMap::new_in(a), building, jobs: VecDeque::new_in(a), entries: SMap::new_in(a), held: 0, unkeyed: 0,
            actions: SVec::new_in(a),
            #[cfg(test)]
            plant: Plant::None,
        };
        for worker in 0..opts.workers {
            relay.feed(now, Event::Free(Worker(worker)))?;
        }
        Ok(relay)
    }

    /// The earliest of the asks' deadlines and the pool's, with what it waits on; `None`: no
    /// ask is open and nothing is being made.
    pub fn wait(&self) -> Option<(Wait, Time)> {
        let code = self.due.first().and_then(|&(due, child)| self.askers[child].asked.map(|asked| (Wait::Code { child: Child(child), key: asked.key }, Time(due))));
        let pool = self.pool.wait().map(|(waiting, until)| (Wait::Pool(waiting), until));
        match (code, pool) {
            (Some(code), Some(pool)) => Some(if code.1.0 <= pool.1.0 { code } else { pool }),
            (code, pool) => code.or(pool),
        }
    }

    /// Whether `child` has an ask open or an answer not taken.
    pub fn asking(&self, child: Child) -> bool {
        self.askers.get(child.0).is_some_and(|asker| asker.asked.is_some() || asker.got.is_some())
    }

    /// Whether `child`'s answer has come and is not taken ([`Relay::answer`]).
    pub fn ready(&self, child: Child) -> bool {
        self.askers.get(child.0).is_some_and(|asker| asker.got.is_some())
    }

    /// `child` asks for the instance under `key` (`None`: unkeyed, its own), to be made by
    /// `order` unless another ask of the key is made or being made.
    pub fn ask(&mut self, now: Time, child: Child, key: Option<CodeKey>, order: J) -> Result<(), RelayError> {
        self.expire(now)?;
        let asker = self.askers.get(child.0).ok_or(RelayError::UnknownChild(child))?;
        if asker.end != End::Live {
            return Err(RelayError::ExitedChild(child));
        }
        if let Some(open) = asker.asked.map(|asked| asked.key).or(match asker.got {
            Some(Got::Made(key) | Got::Refused(key) | Got::Late { key, .. }) => Some(key),
            None => None,
        }) {
            return Err(RelayError::TwoAsks { child, asked: open });
        }
        let (key, unkeyed) = match key {
            Some(key) => (key, false),
            None => (self.fresh(), true),
        };
        #[cfg(test)]
        let key = if self.plant == Plant::MakeTwice { self.fresh() } else { key };
        let due = now.0.saturating_add(self.opts.code_wait);
        self.askers[child.0].asked = Some(Asked { key, due });
        self.due.insert((due, child.0));
        // A key the pool has never seen is made by this ask's order; otherwise it is made or
        // being made already, and this ask only waits for it.
        if self.pool.find(key).is_none() {
            self.orders.insert(key, Ordered { order, unkeyed });
        }
        self.feed(now, Event::Demand { child, thread: Thread(0), key })
    }

    /// The next make for a worker, in the pool's order: the driver hands it to that worker.
    pub fn job(&mut self) -> Option<(Worker, CodeKey, J)> {
        self.jobs.pop_front()
    }

    /// `worker` made its key's entry: `bytes`.
    pub fn made(&mut self, now: Time, worker: Worker, bytes: SVec<'a, u8>) -> Result<(), RelayError> {
        let (key, unkeyed) = self.owned(worker)?;
        // An unkeyed make whose one asker ended is answered to no one: nothing is kept.
        if !unkeyed || self.asked(key) {
            let len = bytes.len();
            if self.held.saturating_add(len) > self.opts.most_bytes {
                return Err(RelayError::Full { key, bytes: len, most: self.opts.most_bytes });
            }
            self.held += len;
            self.entries.insert(key, Entry {
                bytes: Some(bytes), refusal: None, unkeyed,
                #[cfg(test)]
                answered: 0,
            });
        }
        self.feed(now, Event::Finished { worker, key, result: Finished::Made(Record { identity: key.0, direct: &[] }) })?;
        self.feed(now, Event::Free(worker))?;
        self.expire(now)
    }

    /// `worker` refused its key's make: `why`.
    pub fn refused(&mut self, now: Time, worker: Worker, why: E) -> Result<(), RelayError> {
        let (key, unkeyed) = self.owned(worker)?;
        if !unkeyed || self.asked(key) {
            self.entries.insert(key, Entry {
                bytes: None, refusal: Some(why), unkeyed,
                #[cfg(test)]
                answered: 0,
            });
        }
        self.feed(now, Event::Finished { worker, key, result: Finished::Refused(Cause::Build(key.0)) })?;
        self.feed(now, Event::Free(worker))?;
        self.expire(now)
    }

    /// The clock moved: each ask past its deadline is answered late.
    pub fn tick(&mut self, now: Time) -> Result<(), RelayError> {
        self.expire(now)?;
        self.feed(now, Event::Tick)
    }

    /// `child` ended: its open ask is withdrawn, and nothing is answered to it after.
    pub fn exited(&mut self, now: Time, child: Child) -> Result<(), RelayError> {
        let asker = self.askers.get_mut(child.0).ok_or(RelayError::UnknownChild(child))?;
        let end = asker.end;
        if end == End::Exited {
            return Err(RelayError::ExitedChild(child));
        }
        asker.end = End::Exited;
        let untaken = asker.got.take();
        if let Some(asked) = asker.asked.take() {
            self.due.remove(&(asked.due, child.0));
        }
        // An unkeyed answer it never took is no one else's.
        if let Some(Got::Made(key) | Got::Refused(key)) = untaken
            && self.entries.get(&key).is_some_and(|entry| entry.unkeyed)
            && let Some(entry) = self.entries.remove(&key)
        {
            self.held -= entry.bytes.as_ref().map_or(0, |bytes| bytes.len());
        }
        #[cfg(test)]
        if self.plant == Plant::PublishExited {
            return Ok(());
        }
        match end {
            End::Live => self.feed(now, Event::Exited(child)),
            // Withdrawn from the pool when its ask went late.
            End::Late | End::Exited => Ok(()),
        }
    }

    /// `child`'s answer, handed to `with` once; `None`: none has come.
    pub fn answer<R>(&mut self, child: Child, with: impl FnOnce(Reply<'_, E>) -> R) -> Result<Option<R>, RelayError> {
        let asker = self.askers.get_mut(child.0).ok_or(RelayError::UnknownChild(child))?;
        let Some(got) = asker.got.take() else { return Ok(None) };
        let (key, answered) = match got {
            Got::Late { key, waited } => return Ok(Some(with(Reply::Late { key, waited }))),
            Got::Made(key) => {
                let entry = self.entries.get(&key).ok_or(RelayError::NotMade(key))?;
                let bytes = entry.bytes.as_ref().ok_or(RelayError::NotMade(key))?;
                (key, with(Reply::Made(bytes.as_slice())))
            }
            Got::Refused(key) => {
                let entry = self.entries.get(&key).ok_or(RelayError::NotMade(key))?;
                let why = entry.refusal.as_ref().ok_or(RelayError::NotMade(key))?;
                (key, with(Reply::Refused(why)))
            }
        };
        // An unkeyed entry is its one child's: gone once taken.
        if self.entries.get(&key).is_some_and(|entry| entry.unkeyed)
            && let Some(entry) = self.entries.remove(&key)
        {
            self.held -= entry.bytes.as_ref().map_or(0, |bytes| bytes.len());
        }
        Ok(Some(answered))
    }

    fn fresh(&mut self) -> CodeKey {
        self.unkeyed += 1;
        CodeKey(ContentId::of(UNKEYED, &self.unkeyed.to_le_bytes()))
    }

    fn owned(&mut self, worker: Worker) -> Result<(CodeKey, bool), RelayError> {
        self.building.get_mut(worker.0).and_then(Option::take).ok_or(RelayError::NotOwner { worker })
    }

    /// Whether a live child's ask of `key` is open (an unkeyed key has one asker at most).
    fn asked(&self, key: CodeKey) -> bool {
        self.askers.iter().any(|asker| asker.end == End::Live && asker.asked.is_some_and(|asked| asked.key == key))
    }

    fn expire(&mut self, now: Time) -> Result<(), RelayError> {
        while let Some(&(due, child)) = self.due.first() {
            if due > now.0 {
                break;
            }
            self.due.remove(&(due, child));
            let asker = &mut self.askers[child];
            let Some(asked) = asker.asked.take() else { continue };
            let waited = now.0.saturating_sub(due.saturating_sub(self.opts.code_wait));
            asker.got = Some(Got::Late { key: asked.key, waited });
            asker.end = End::Late;
            // Withdrawn: the pool answers it nothing more (its make, if begun, still returns once).
            self.feed(now, Event::Exited(Child(child)))?;
        }
        Ok(())
    }

    fn feed(&mut self, now: Time, event: Event<'_>) -> Result<(), RelayError> {
        let mut actions = core::mem::replace(&mut self.actions, SVec::new_in(self.opts.alloc));
        actions.clear();
        let fed = self.feed_in(now, event, &mut actions);
        actions.clear();
        self.actions = actions;
        fed
    }

    fn feed_in(&mut self, now: Time, event: Event<'_>, actions: &mut SVec<'a, Action>) -> Result<(), RelayError> {
        self.pool.step(now, event, actions).map_err(RelayError::Pool)?;
        let mut at = 0;
        while at < actions.len() {
            let action = actions[at];
            at += 1;
            match action {
                // The child's walk missed it in the store before it asked: made, never looked up again.
                Action::Lookup(key) => self.pool.step(now, Event::Looked { key, result: Ok(None) }, actions).map_err(RelayError::Pool)?,
                Action::Build { worker, key, .. } => {
                    let Ordered { order, unkeyed } = self.orders.remove(&key).ok_or(RelayError::NoOrder(key))?;
                    self.building[worker.0] = Some((key, unkeyed));
                    self.jobs.push_back((worker, key, order));
                }
                Action::AwaitProducer { key, .. } => return Err(RelayError::Producer(key)),
                // Only speculation is cancelled, and the relay speculates nothing.
                Action::Cancel(_) => {}
                Action::Publish { demand, .. } => self.set(demand.child, Got::Made(demand.key))?,
                Action::Refuse { demand, .. } => self.set(demand.child, Got::Refused(demand.key))?,
            }
        }
        Ok(())
    }

    fn set(&mut self, child: Child, got: Got) -> Result<(), RelayError> {
        let key = match got {
            Got::Made(key) | Got::Refused(key) | Got::Late { key, .. } => key,
        };
        #[cfg(test)]
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.answered += 1;
            if self.plant == Plant::DroppedWake && entry.answered > 1 {
                return Ok(());
            }
        }
        let asker = self.askers.get_mut(child.0).ok_or(RelayError::UnknownChild(child))?;
        match asker.asked {
            Some(asked) if asked.key == key && asker.end == End::Live => {
                asker.asked = None;
                asker.got = Some(got);
                self.due.remove(&(asked.due, child.0));
                Ok(())
            }
            _ => Err(RelayError::NotAsking { child, key }),
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plant {
    None,
    /// The second and later waiters of a key are never answered.
    DroppedWake,
    /// An ended child is not withdrawn from the pool, so its make's answer is published to it.
    PublishExited,
    /// Each keyed ask gets a key of its own, so one key is made once per asker.
    MakeTwice,
}

/// The marker blaze-sim's `Plant` extends (blaze_own `planted`): the relay simulation's
/// `plants!` implements `Plant` over this enum (code_pool_relay_sim.rs).
#[cfg(test)]
impl blaze_own::planted::Plants for Plant {}

#[cfg(test)]
#[path = "code_pool_relay_sim.rs"]
mod tests;
