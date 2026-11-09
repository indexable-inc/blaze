//! A scheduler as a value (docs/v2/plan/design/fibers.md): each pool, fiber driver, client thread
//! and [`super::Event`] holds a [`Scheduler`], a wait graph and its members' wakers behind a lock
//! of its own, so pools whose jobs never meet never share a lock.
//!
//! Two schedulers whose jobs meet join, for good: a wait on a key, a counter or a job of
//! another, a count towards another's counter, a claim for another's job, a scope opened on
//! another's pool, a pool's stop by another's job. The smaller's world moves into the bigger's
//! ([`super::World::absorb`]) and the smaller points at the bigger from then on (a union-find
//! by rank, whose sets never split). So a wait is always checked against every wait it could
//! close a cycle with, and woken by whatever ends it, as when the process had one scheduler; a
//! stall is found per scheduler, among the waits that can only end by each other.
//!
//! Who holds what lock: a scheduler's lock, then a fiber driver's own; two schedulers' locks
//! only while joining, in the order of their ids. Job, counter and member ids are the
//! process's ([`super::ids`]), so a job's scheduler is found from its id with no lock, and a
//! key's flight from the process's key index.

use std::{
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
};

use blaze_core::lock::LockGuard;
use blaze_hash::hash_map;

use super::{
    CounterId, JobId, Wait, World,
    fiber::{Io, OwnIo},
    ids,
};
use crate::Key;

/// How many times a lock or join may find the scheduler it locked joined into another
/// meanwhile before it gives up: each retry means a join happened, and joins are bounded by
/// the schedulers there are.
const FUEL: u32 = 1 << 20;

/// A scheduler: a wait graph, its members and their wakers, behind one lock. Cloned, it is the
/// same scheduler.
#[derive(Clone)]
pub(crate) struct Scheduler(Arc<Cell>);

/// A scheduler's state, and the one it joined, if it did.
pub(crate) struct Cell {
    /// Its id in the process ([`ids::take_domain`]): the order two are locked in to join.
    id: u32,
    /// The scheduler it joined, set once, under both locks: its world is there from then on.
    #[expect(
        clippy::disallowed_types,
        reason = "set once as two schedulers join, read with no lock by every lock"
    )]
    parent: std::sync::OnceLock<Arc<Cell>>,
    /// The union's rank: the smaller joins the bigger, so a chain is at most log2 of the
    /// schedulers long.
    rank: AtomicU32,
    #[expect(
        clippy::disallowed_types,
        reason = "the scheduler's one lock: held only to change its world, never across a call into work"
    )]
    core: blaze_core::lock::Lock<Domain>,
    /// Jobs a stall or a pool's stop woke to fail that were not told yet: while 0, a wait that
    /// ends asks nothing more ([`super::host`]). Of the scheduler at the root.
    pub(super) failing: AtomicUsize,
    /// When a pool job's timed wait is next due, on [`super::now`]'s clock (`u64::MAX` for
    /// none): set under the lock, read by workers without it. Of the scheduler at the root.
    pub(super) deadline: AtomicU64,
}

/// What a scheduler's lock guards: the [`World`] (its wait graph, each driver's placement and
/// each pool's or client's jobs as members) and each member's wakers. A job of one member woken
/// by another's is woken through them.
pub(crate) struct Domain {
    pub(super) world: World,
    /// Each fiber driver's wakers, by member.
    pub(super) io: Vec<(u32, Io)>,
    /// How each member of its own placement (a pool, a client thread) takes back a woken job.
    pub(super) own: Vec<(u32, OwnIo)>,
    pub(super) dirty: Vec<u32>,
    pub(super) woken: Vec<usize>,
    /// Keys whose flights ended, to leave the process's index as the lock is let go.
    unindex: Vec<Key>,
}

impl Drop for Cell {
    fn drop(&mut self) {
        ids::unregister(self.id);
    }
}

#[cfg(test)]
thread_local! {
    /// How many times this thread took a scheduler's lock.
    pub(crate) static LOCKS_TAKEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl Scheduler {
    /// A scheduler of its own, with room for `jobs` before its columns grow.
    pub(crate) fn new(jobs: usize) -> Scheduler {
        let id = ids::take_domain();
        // alloc: a scheduler, once per pool, driver, client thread or event
        #[expect(clippy::disallowed_types, reason = "makes `Cell::core`, which allows it")]
        let cell = Arc::new(Cell {
            id,
            parent: std::sync::OnceLock::new(),
            rank: AtomicU32::new(0),
            core: blaze_core::lock::Lock::new("sched.domain.core", Domain {
                world: World::new(jobs, id),
                io: Vec::new(),
                own: Vec::with_capacity(1),
                dirty: Vec::with_capacity(4),
                woken: Vec::with_capacity(16),
                unindex: Vec::new(),
            }),
            failing: AtomicUsize::new(0),
            deadline: AtomicU64::new(u64::MAX),
        });
        ids::register(id, &cell);
        Scheduler(cell)
    }

    pub(super) fn cell(&self) -> &Arc<Cell> {
        &self.0
    }

    /// Locked: the scheduler it is joined into, if it was.
    pub(super) fn lock(&self) -> Guard<'_> {
        lock(&self.0)
    }

    /// The scheduler at the root of its joins, whose state is its own now: for its counters.
    pub(super) fn root(&self) -> &Cell {
        self.0.root()
    }
}

impl Cell {
    /// The scheduler at the root of this one's joins: itself if it joined none.
    pub(super) fn root(self: &Arc<Cell>) -> &Arc<Cell> {
        let mut at = self;
        while let Some(parent) = at.parent.get() {
            at = parent;
        }
        at
    }
}

/// A scheduler, locked: its [`Domain`]. As it is let go, the keys whose flights ended under it
/// leave the process's index.
pub(crate) struct Guard<'a> {
    cell: &'a Arc<Cell>,
    domain: LockGuard<'a, Domain>,
}

impl Guard<'_> {
    /// The scheduler locked (a root).
    pub(super) fn cell(&self) -> &Arc<Cell> {
        self.cell
    }

    /// `by` (running, or a driver's or an event's for `None`) makes `key`'s value: `false` when
    /// the key has a flight already, in this scheduler or any other.
    pub(super) fn claim(&mut self, key: Key, by: Option<JobId>) -> bool {
        if !self.index(key) {
            return false;
        }
        let claimed = self.domain.world.claim(key, by);
        assert!(
            claimed,
            "key {key:?} has a flight in scheduler {} that the process's index did not have",
            self.cell.id
        );
        true
    }

    /// As [`super::World::claim_for`], for a key no scheduler has a flight of.
    pub(super) fn claim_for(&mut self, key: Key, job: JobId, scope: Option<CounterId>) -> bool {
        if !self.index(key) {
            return false;
        }
        let claimed = self.domain.world.claim_for(key, job, scope);
        assert!(
            claimed,
            "key {key:?} has a flight in scheduler {} that the process's index did not have",
            self.cell.id
        );
        true
    }

    /// Puts `key` in the process's index as this scheduler's: `false` when it has a flight.
    fn index(&self, key: Key) -> bool {
        match index(key).lock().entry(key) {
            hash_map::Entry::Occupied(_) => false,
            hash_map::Entry::Vacant(it) => {
                it.insert(Arc::clone(self.cell)); // clone: a reference count, per flight
                true
            }
        }
    }
}

impl Deref for Guard<'_> {
    type Target = Domain;

    fn deref(&self) -> &Domain {
        &self.domain
    }
}

impl DerefMut for Guard<'_> {
    fn deref_mut(&mut self) -> &mut Domain {
        &mut self.domain
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let domain = &mut *self.domain;
        domain.world.take_freed_keys(&mut domain.unindex);
        if domain.unindex.is_empty() {
            return;
        }
        for key in domain.unindex.drain(..) {
            index(key).lock().remove(&key);
        }
    }
}

/// Shards of the key index, each behind a lock of its own: a flight's start and end take one.
const SHARDS: usize = 128;

#[expect(
    clippy::disallowed_types,
    reason = "a shard of the process's index of flights: held to add, find or drop one key"
)]
type Shard = blaze_core::lock::Lock<blaze_hash::HashMap<Key, Arc<Cell>, std::alloc::Global>>;

/// The shard of the process's index of flights that has `key`: every key with a flight now,
/// and the scheduler that has it (or one joined into it), so a key has one flight in all the
/// process's schedulers. Locks, not a lock-free map: a client thread's last flight leaves it
/// as the thread's locals are destroyed, where an epoch's thread-local may be gone already.
fn index(key: Key) -> &'static Shard {
    #[expect(clippy::disallowed_types, reason = "the process's one index of flights, made once")]
    static INDEX: std::sync::OnceLock<Box<[Shard]>> = std::sync::OnceLock::new();
    #[expect(clippy::disallowed_types, reason = "makes the shards `Shard` allows")]
    let shards = INDEX.get_or_init(|| {
        // alloc: the index, once, each shard with room for a few flights so that a warm
        // process's flights allocate nothing (about 40 KiB)
        (0..SHARDS)
            .map(|_| {
                blaze_core::lock::Lock::new(
                    "sched.flights.index",
                    blaze_hash::HashMap::with_capacity_in(4, std::alloc::Global),
                )
            })
            .collect()
    });
    let hash = std::hash::BuildHasher::hash_one(&blaze_hash::FixedState, key);
    &shards[(hash >> 57) as usize % SHARDS]
}

/// The scheduler with a flight of `key` now, if one has.
pub(super) fn key_cell(key: Key) -> Option<Arc<Cell>> {
    index(key).lock().get(&key).cloned() // clone: a reference count
}

/// The scheduler `job` is in now: none once it ended.
pub(super) fn job_cell(job: JobId) -> Option<Arc<Cell>> {
    ids::domain(ids::job_domain(job))
}

/// The scheduler `job` (live) is in now.
///
/// # Panics
///
/// Where `job` is in none: it is not live.
pub(super) fn live_job_cell(job: JobId) -> Arc<Cell> {
    job_cell(job).unwrap_or_else(|| panic!("job {} is in no live scheduler: it is not live", job.0))
}

/// The scheduler `counter` (live) is in now.
///
/// # Panics
///
/// Where `counter` is in none: it is not live.
pub(super) fn counter_cell(counter: CounterId) -> Arc<Cell> {
    let id = ids::counter_domain(counter);
    ids::domain(id).unwrap_or_else(|| {
        panic!("counter {} is in no live scheduler (its word says {id}): it is not live", counter.0)
    })
}

/// The scheduler that has what `on` waits for now, if one has: a key's flight, a counter, a
/// live job.
pub(super) fn wait_cell(on: Wait) -> Option<Arc<Cell>> {
    match on {
        Wait::Key(key) => key_cell(key),
        Wait::Counter(counter) => Some(counter_cell(counter)),
        // A job that ended is in none: the wait is ready.
        Wait::Job(job) => job_cell(job),
        Wait::Until(_) => None,
    }
}

/// `cell`'s lock: a contended acquire waits in the lock's own span, which names the holder.
fn take(cell: &Cell) -> LockGuard<'_, Domain> {
    #[cfg(test)]
    LOCKS_TAKEN.with(|it| it.set(it.get() + 1));
    cell.core.lock()
}

/// The scheduler `cell` is joined into, locked.
///
/// # Panics
///
/// Past [`FUEL`] tries that each found it joined into another as it was locked.
pub(super) fn lock(cell: &Arc<Cell>) -> Guard<'_> {
    for _ in 0..FUEL {
        let root = cell.root();
        let domain = take(root);
        if root.parent.get().is_none() {
            return Guard { cell: root, domain };
        }
    }
    panic!("scheduler {} was joined into another each of {FUEL} times it was locked", cell.id);
}

/// `mine`, locked, joined first with the scheduler `other` names (read again under the lock,
/// as it may change until then; `None` for none to join).
///
/// # Panics
///
/// Past [`FUEL`] joins that each found `other` moved on.
pub(super) fn lock_joined<'a>(
    mine: &'a Arc<Cell>,
    other: impl Fn() -> Option<Arc<Cell>>,
) -> Guard<'a> {
    for _ in 0..FUEL {
        let guard = lock(mine);
        let Some(theirs) = other() else { return guard };
        if Arc::ptr_eq(theirs.root(), guard.cell) {
            return guard;
        }
        drop(guard);
        join(mine, &theirs);
    }
    panic!(
        "scheduler {} joined another {FUEL} times and still was not joined with what it waits on",
        mine.id
    );
}

/// `a` and `b` are one scheduler from now on: the root of smaller rank (the higher id on a
/// tie) moves into the other's.
///
/// # Panics
///
/// Past [`FUEL`] tries that each found one of them joined into a third meanwhile.
fn join(a: &Arc<Cell>, b: &Arc<Cell>) {
    for _ in 0..FUEL {
        let (ra, rb) = (a.root(), b.root());
        if Arc::ptr_eq(ra, rb) {
            return;
        }
        let (first, second) = if ra.id < rb.id { (ra, rb) } else { (rb, ra) };
        let mut held_first = take(first);
        let mut held_second = take(second);
        if first.parent.get().is_some() || second.parent.get().is_some() {
            continue;
        }
        let (rank_first, rank_second) =
            (first.rank.load(Ordering::Relaxed), second.rank.load(Ordering::Relaxed));
        let (big, small, big_domain, small_domain) = if rank_first >= rank_second {
            (first, second, &mut *held_first, &mut *held_second)
        } else {
            (second, first, &mut *held_second, &mut *held_first)
        };
        if rank_first == rank_second {
            big.rank.store(rank_first + 1, Ordering::Relaxed);
        }
        absorb(big, big_domain, small, small_domain);
        let set = small.parent.set(Arc::clone(big)); // clone: a reference count, once per join
        assert!(set.is_ok(), "scheduler {} joined twice", small.id);
        return;
    }
    panic!(
        "schedulers {} and {} were each joined into a third {FUEL} times as they joined",
        a.id, b.id
    );
}

/// `small`'s state moves into `big`'s, both locked: its world, wakers, failures not yet told
/// and timed waits.
fn absorb(big: &Cell, into: &mut Domain, small: &Cell, from: &mut Domain) {
    let world = std::mem::replace(&mut from.world, World::new(0, small.id));
    into.world.absorb(world);
    into.io.append(&mut from.io);
    into.own.append(&mut from.own);
    into.dirty.append(&mut from.dirty);
    into.unindex.append(&mut from.unindex);
    big.failing.fetch_add(small.failing.load(Ordering::Acquire), Ordering::AcqRel);
    big.deadline.store(into.world.next_own_deadline().unwrap_or(u64::MAX), Ordering::Release);
}

/// Every scheduler of the process that joined none, by id, for a process-wide report.
pub(super) fn roots() -> Vec<Arc<Cell>> {
    let mut all = ids::domains();
    all.retain(|it| it.parent.get().is_none());
    all
}
