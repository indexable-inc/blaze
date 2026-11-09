//! The process's job, counter, member and scheduler ids (docs/v2/plan/design/fibers.md): one
//! space for every [`super::Scheduler`] of the process, so two schedulers that join
//! ([`super::Scheduler::join`]) never hold two jobs, counters or members of one id, and a job's
//! or counter's scheduler is found from its id with no lock.
//!
//! A job id's word holds its generation (bumped at each end, so a stale handle tells an old job
//! from a new one) and the id of the scheduler it is in now; a counter id's word the latter.
//! Each word is written only by whoever holds the lock of the scheduler the id is in (or, for a
//! free id, by the one thread that took it from the free list), and read by anyone.
//!
//! Nothing here keeps a thread-local with a destructor (a thread's free-list shard is a number):
//! a client thread's scheduler goes as the thread's locals are destroyed ([`super::host`]'s
//! `Client`), in whatever order the platform runs them, and frees its ids then.

use std::sync::{
    Arc, Weak,
    atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use super::{CounterId, FIRST_GENERATION, JobId, MAX_JOBS, domain::Cell};

/// Slots a chunk of a [`Table`] holds.
const CHUNK: usize = 1 << 14;
/// Chunks a table has room for: [`MAX_JOBS`] ids.
const CHUNKS: usize = MAX_JOBS as usize / CHUNK;

/// A slot per id, in chunks made as ids reach them and kept for the process's life.
struct Table<T> {
    chunks: [AtomicPtr<[T; CHUNK]>; CHUNKS],
    /// A slot of an id no one took yet.
    empty: fn() -> T,
}

type Words = Table<AtomicU64>;

impl<T> Table<T> {
    const fn new(empty: fn() -> T) -> Table<T> {
        Table { chunks: [const { AtomicPtr::new(std::ptr::null_mut()) }; CHUNKS], empty }
    }

    /// The slot of `id`, its chunk made if it was not.
    #[allow(unsafe_code)]
    fn word(&self, id: u32) -> &T {
        let (chunk, at) = (id as usize / CHUNK, id as usize % CHUNK);
        assert!(chunk < CHUNKS, "id {id} is past the scheduler's {MAX_JOBS} ids");
        let slot = &self.chunks[chunk];
        let mut ptr = slot.load(Ordering::Acquire);
        if ptr.is_null() {
            // alloc: a chunk of the process's ids, once per 16384 ids, kept for its life; made on
            // the heap, never on the (fiber's) stack
            let slots: Box<[T]> = (0..CHUNK).map(|_| (self.empty)()).collect();
            let slots: Box<[T; CHUNK]> =
                slots.try_into().unwrap_or_else(|_| unreachable!("a chunk is CHUNK slots"));
            let made = Box::into_raw(slots);
            ptr = match slot.compare_exchange(
                std::ptr::null_mut(),
                made,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => made,
                Err(theirs) => {
                    // SAFETY: `made` came from `Box::into_raw` above and was published nowhere.
                    drop(unsafe { Box::from_raw(made) });
                    theirs
                }
            };
        }
        // SAFETY: a chunk, once published, is never freed or moved: the table is a static.
        let chunk: &[T; CHUNK] = unsafe { &*ptr };
        &chunk[at]
    }
}

/// Free-list shards of a [`Pool`]: a thread frees to and takes from its own, so pools'
/// threads share no lock on a spawn or an end.
const FREE_SHARDS: usize = 64;

/// The free-list shard each thread uses, handed out in turn.
static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// This thread's free-list shard, `usize::MAX` until its first. A number with no
    /// destructor, so it reads the same while the thread's other locals are destroyed.
    static SHARD: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
}

fn shard() -> usize {
    SHARD.with(|it| {
        if it.get() == usize::MAX {
            it.set(NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % FREE_SHARDS);
        }
        it.get()
    })
}

/// A pool of ids: a free one first (this thread's shard's, then any other's), then a fresh one.
/// A shard's list grows only past the most ids ever free in it at once, so a warm thread's
/// take and free allocate nothing (a lock-free queue's would: a block per few frees).
struct Pool {
    #[expect(
        clippy::disallowed_types,
        reason = "a thread's shard of free ids: held for one push or pop, never across a call"
    )]
    free: [blaze_core::lock::Lock<Vec<u32>>; FREE_SHARDS],
    /// Ids in `free`, over every shard: none, and a take goes fresh without visiting them.
    freed: AtomicUsize,
    next: AtomicU32,
}

impl Pool {
    /// Ids from `first` up.
    #[expect(clippy::disallowed_types, reason = "makes `Pool::free`, which allows it")]
    const fn new(first: u32) -> Pool {
        Pool {
            free: [const { blaze_core::lock::Lock::new("sched.ids.free", Vec::new()) }; FREE_SHARDS],
            freed: AtomicUsize::new(0),
            next: AtomicU32::new(first),
        }
    }

    fn take(&self, what: &str) -> u32 {
        if self.freed.load(Ordering::Acquire) != 0 {
            let home = shard();
            for at in (0..FREE_SHARDS).map(|it| (home + it) % FREE_SHARDS) {
                if let Some(id) = self.free[at].lock().pop() {
                    self.freed.fetch_sub(1, Ordering::Relaxed);
                    return id;
                }
            }
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(id < MAX_JOBS, "the process ran out of {what} ids: {MAX_JOBS} are live at once");
        id
    }

    /// `id` is free to take again.
    fn free(&self, id: u32) {
        // alloc: a shard's list grows past the most ids ever free in it at once, then never
        self.free[shard()].lock().push(id);
        self.freed.fetch_add(1, Ordering::Release);
    }
}

fn zero() -> AtomicU64 {
    AtomicU64::new(0)
}

static JOBS: Words = Words::new(zero);
static JOB_IDS: Pool = Pool::new(0);
static COUNTERS: Words = Words::new(zero);
static COUNTER_IDS: Pool = Pool::new(0);
static MEMBERS: AtomicU32 = AtomicU32::new(0);
/// Scheduler ids from 1: 0 is a job's or counter's word of none.
static DOMAIN_IDS: Pool = Pool::new(1);

/// The low half of a job's word: the scheduler it is in. The high half: its generation less
/// [`FIRST_GENERATION`], so a word never written reads as the first generation.
fn pack(generation: u32, domain: u32) -> u64 {
    u64::from(generation - FIRST_GENERATION) << 32 | u64::from(domain)
}

fn generation_of(word: u64) -> u32 {
    (word >> 32) as u32 + FIRST_GENERATION
}

/// A job id no live job has, in scheduler `domain`.
pub(super) fn take_job(domain: u32) -> JobId {
    let id = JOB_IDS.take("job");
    let word = JOBS.word(id);
    word.store(pack(generation_of(word.load(Ordering::Relaxed)), domain), Ordering::Release);
    JobId(id)
}

/// `job` ended: its generation moves on and its id is free.
pub(super) fn free_job(job: JobId) {
    let word = JOBS.word(job.0);
    let generation = super::next_generation(generation_of(word.load(Ordering::Relaxed)));
    word.store(pack(generation, 0), Ordering::Release);
    JOB_IDS.free(job.0);
}

/// `job`'s generation now: [`FIRST_GENERATION`] for an id never used.
pub(super) fn generation(job: JobId) -> u32 {
    generation_of(JOBS.word(job.0).load(Ordering::Acquire))
}

/// The scheduler `job` is in, by id: 0 for none (an id not live).
pub(super) fn job_domain(job: JobId) -> u32 {
    JOBS.word(job.0).load(Ordering::Acquire) as u32
}

/// `job`, live, is in scheduler `domain` from now on: under both schedulers' locks.
pub(super) fn move_job(job: JobId, domain: u32) {
    let word = JOBS.word(job.0);
    word.store(pack(generation_of(word.load(Ordering::Relaxed)), domain), Ordering::Release);
}

/// A counter id no live counter has, in scheduler `domain`.
pub(super) fn take_counter(domain: u32) -> CounterId {
    let id = COUNTER_IDS.take("counter");
    COUNTERS.word(id).store(u64::from(domain), Ordering::Release);
    CounterId(id)
}

pub(super) fn free_counter(counter: CounterId) {
    COUNTERS.word(counter.0).store(0, Ordering::Release);
    COUNTER_IDS.free(counter.0);
}

pub(super) fn counter_domain(counter: CounterId) -> u32 {
    COUNTERS.word(counter.0).load(Ordering::Acquire) as u32
}

pub(super) fn move_counter(counter: CounterId, domain: u32) {
    COUNTERS.word(counter.0).store(u64::from(domain), Ordering::Release);
}

/// A member number no other member of the process ever had, with `bits` (`OWN`, `CLIENT`) set.
pub(super) fn take_member(bits: u32) -> u32 {
    let member = MEMBERS.fetch_add(1, Ordering::Relaxed);
    assert!(member & (super::OWN | super::CLIENT) == 0, "the process ran out of scheduler members");
    member | bits
}

/// Every scheduler of the process by id, for a job's or counter's id to find its own, and the
/// process-wide reports ([`super::waits_now`]) to visit each: a slot each, behind a lock of
/// its own, which only that scheduler's making, dropping and lookups take.
#[expect(
    clippy::disallowed_types,
    reason = "a scheduler's slot: taken only as it is made, dropped or looked up by id"
)]
static DOMAINS: Table<blaze_core::lock::Lock<Weak<Cell>>> =
    Table::new(|| blaze_core::lock::Lock::new("sched.ids.domain_slot", Weak::new()));

/// A scheduler id no live scheduler has: one that dropped may be taken again. A stale lookup
/// (of a job that ended as it was looked up, its scheduler gone and its id taken again) finds
/// the new scheduler, which a join with it only makes coarser.
pub(super) fn take_domain() -> u32 {
    DOMAIN_IDS.take("scheduler")
}

pub(super) fn register(id: u32, cell: &Arc<Cell>) {
    *DOMAINS.word(id).lock() = Arc::downgrade(cell);
}

/// The scheduler of `id` dropped: its id is free.
pub(super) fn unregister(id: u32) {
    *DOMAINS.word(id).lock() = Weak::new();
    DOMAIN_IDS.free(id);
}

/// The scheduler of id `id`, if it lives.
pub(super) fn domain(id: u32) -> Option<Arc<Cell>> {
    if id == 0 {
        return None;
    }
    DOMAINS.word(id).lock().upgrade()
}

/// Every scheduler that lives now, by id.
pub(super) fn domains() -> Vec<Arc<Cell>> {
    let taken = DOMAIN_IDS.next.load(Ordering::Acquire);
    // alloc: a process-wide report, off every hot path
    (1..taken).filter_map(|id| DOMAINS.word(id).lock().upgrade()).collect()
}
