//! Threads, the seeded scheduler, data races and weak memory: a port of Miri's
//! `src/tools/miri/src/concurrency/` (`vector_clock.rs`, `data_race.rs`, `weak_memory.rs`,
//! `scheduler.rs`, the thread states of `thread.rs`, and the happens-before edges of `sync.rs`
//! and `init_once.rs`).
//!
//! Every thread has a vector clock; every byte of every allocation has the clocks of its last
//! write, its reads since, and, once accessed atomically, its atomic accesses. An access races
//! when it is on another thread, one of the two is non-atomic or they are atomics of different
//! sizes, one is a write, and neither happens before the other. Allocation and deallocation
//! are writes. Under weak memory an atomic load may read an older store of the same location
//! (`weak.rs`).
//!
//! How the checker drives it, per evaluator event:
//!
//! - an allocation: [`Races::new_alloc`]; a free: [`Races::free`] (a write of every byte);
//! - a non-atomic access: [`Races::read`] or [`Races::write`] with `atomic: None`; a retag's
//!   implicit access: [`Races::retag`];
//! - an atomic load: [`Races::read`] with its ordering, then [`Races::load_value`] for the bytes
//!   it reads when weak memory picked an older store;
//! - an atomic store: [`Races::write`] with its ordering, then [`Races::store_value`];
//! - an atomic read-modify-write: [`Races::rmw`], then [`Races::store_value`]; a failed
//!   compare-exchange: [`Races::failed_cxchg`] (it reads the latest store, as Miri's does);
//! - a fence: [`Races::fence`]; a futex wake, unlock or notify: [`Races::release`] of the object,
//!   and [`Races::acquire`] when the woken or locking thread goes on;
//! - threads: [`Races::spawn`], [`Races::exit`], [`Races::join`] (after the joined thread
//!   exited), [`Races::detach`], [`Races::yield_active`] at `sched_yield`, `thread::yield_now`
//!   and spin-loop hints, and [`Races::schedule`] at every terminator and whenever the running
//!   thread blocks or ends.
//!
//! Until a second thread is spawned nothing can race, so no per-byte clocks exist: an
//! allocation made then is only its size, and gets its clocks (all written at time zero, which
//! everything later happens after) on its first access once there are threads (Miri's
//! `multi_threaded` flag). Allocations are a column by [`AllocId::index`]; the clocks of one
//! are a sorted run map, one binary search per access. Miri keeps each allocation's clocks
//! behind a `RefCell` in its allocation map.
//!
//! Miri also keeps clocks for locals that have no memory yet (`FrameState`); blaze's evaluator
//! gives every local its memory at `StorageLive`, so locals are ordinary allocations here.

mod cells;
mod error;
mod range_map;
mod vclock;
mod weak;

#[cfg(test)]
mod tests;

pub use error::{Action, DataRace, Deadlock, RaceError, RaceErrorKind, RacingOp};

use blaze_hash::HashMap;
use blaze_ids::rng::SplitMix64;

use crate::ids::{AccessKind, AllocId, AtomicOrd, SiteId, Span, ThreadId};

use self::{
    cells::{AccessType, MemoryCellClocks, NaWriteType, ThreadClockSet},
    range_map::RangeMap,
    vclock::{NaReadType, VClock, VTimestamp, VectorIdx, is_dummy},
    weak::{StoreBuffers, Value},
};

/// How the run's threads are interleaved and whether atomics show weak behavior.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// The seed of every random choice: preemptions and the store a weak load reads.
    pub seed: u64,
    /// The chance of switching threads at each terminator (Miri: `-Zmiri-preemption-rate`).
    pub preemption_rate: f64,
    /// Whether an atomic load may read an older store (Miri:
    /// `-Zmiri-disable-weak-memory-emulation` turns it off).
    pub weak_memory: bool,
}

impl Default for Config {
    /// Miri's defaults (`src/tools/miri/src/eval.rs`, `MiriConfig::default`).
    fn default() -> Config {
        Config { seed: 0, preemption_rate: 0.01, weak_memory: true }
    }
}

/// How often a `compare_exchange_weak` that would succeed fails anyway (Miri:
/// `cmpxchg_weak_failure_rate`).
const CMPXCHG_WEAK_FAILURE_RATE: f64 = 0.8;

/// Miri's `ThreadState` without `Blocked`: the evaluator knows which threads are blocked and
/// passes the others to [`Races::schedule`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThreadState {
    Enabled,
    Terminated,
}

/// Miri: `ThreadJoinStatus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JoinStatus {
    Joinable,
    /// Its handle was dropped; nobody may join it.
    Detached,
    /// Joined once; joining again is undefined.
    Joined,
}

/// Every thread of the run, by [`ThreadId::index`].
#[derive(Debug, Default)]
struct Threads {
    state: Vec<ThreadState>,
    join: Vec<JoinStatus>,
    name: Vec<Option<Box<str>>>,
    /// The thread's vector index; `None` once another thread reuses it (Miri:
    /// `ThreadExtraState::vector_index`).
    vector_index: Vec<Option<VectorIdx>>,
    /// The clock at its exit, which a join acquires (the index may be reused by then).
    termination_clock: Vec<Option<VClock>>,
}

impl Threads {
    fn push(&mut self, name: Option<Box<str>>) -> ThreadId {
        let id = ThreadId(self.state.len() as u32);
        self.state.push(ThreadState::Enabled);
        self.join.push(JoinStatus::Joinable);
        self.name.push(name);
        self.vector_index.push(None);
        self.termination_clock.push(None);
        id
    }

    fn live_count(&self) -> usize {
        self.state.iter().filter(|&&s| s != ThreadState::Terminated).count()
    }

    /// Miri: `Thread::thread_display_name`.
    fn display_name(&self, thread: ThreadId) -> String {
        match &self.name[thread.index()] {
            Some(name) => name.to_string(),
            None => format!("unnamed-{}", thread.0),
        }
    }
}

/// The clocks of every thread and the data race detector's global state (Miri:
/// `data_race::GlobalState`).
#[derive(Debug)]
struct Global {
    /// A second thread was spawned, and not every thread since joined: races are possible.
    multi_threaded: bool,
    vector_clocks: Vec<ThreadClockSet>,
    /// The thread each vector index stands for now.
    vector_info: Vec<ThreadId>,
    threads: Threads,
    /// Indices of exited threads, reusable once every clock has caught up with them.
    reuse_candidates: Vec<VectorIdx>,
    /// SC fences act like RMWs of one global location: they release into and acquire from this.
    last_sc_fence: VClock,
    /// The time of each thread's last SC write; a thread only sets its own index.
    last_sc_write_per_thread: VClock,
    active: ThreadId,
}

/// Two distinct elements of `v`, mutably.
fn pick2_mut<T>(v: &mut [T], a: usize, b: usize) -> (&mut T, &mut T) {
    assert_ne!(a, b);
    if a < b {
        let (lo, hi) = v.split_at_mut(b);
        (&mut lo[a], &mut hi[0])
    } else {
        let (lo, hi) = v.split_at_mut(a);
        (&mut hi[0], &mut lo[b])
    }
}

impl Global {
    /// The main thread alone, at vector index 0 (Miri: `GlobalState::new`).
    fn new() -> Global {
        let mut threads = Threads::default();
        let main = threads.push(Some("main".into()));
        threads.vector_index[main.index()] = Some(VectorIdx(0));
        Global {
            multi_threaded: false,
            vector_clocks: vec![ThreadClockSet::default()],
            vector_info: vec![main],
            threads,
            reuse_candidates: Vec::new(),
            last_sc_fence: VClock::default(),
            last_sc_write_per_thread: VClock::default(),
            active: main,
        }
    }

    #[inline]
    fn thread_index(&self, thread: ThreadId) -> VectorIdx {
        self.threads.vector_index[thread.index()].expect("thread has no assigned vector")
    }

    #[inline]
    fn active_index(&self) -> VectorIdx {
        self.thread_index(self.active)
    }

    /// A vector index no clock can still race with: its thread exited and every clock has
    /// caught up with its last time, or belongs to an exited thread too (Miri:
    /// `find_vector_index_reuse_candidate`).
    fn find_vector_index_reuse_candidate(&mut self) -> Option<VectorIdx> {
        let pos = self.reuse_candidates.iter().position(|&candidate| {
            let target_timestamp = self.vector_clocks[candidate.index()].clock[candidate];
            self.vector_clocks.iter().enumerate().all(|(clock_idx, clock)| {
                clock.clock[candidate] >= target_timestamp
                    || self.reuse_candidates.contains(&VectorIdx(clock_idx as u32))
            })
        })?;
        Some(self.reuse_candidates.swap_remove(pos))
    }

    /// Spawning: what the parent did happens before the child's first step (Miri:
    /// `thread_created`).
    fn thread_created(&mut self, thread: ThreadId, site: SiteId) {
        let current_index = self.active_index();
        self.multi_threaded = true;

        let created_index = if let Some(reuse_index) = self.find_vector_index_reuse_candidate() {
            self.vector_clocks[reuse_index.index()].increment_clock(reuse_index, site);
            let old_thread = self.vector_info[reuse_index.index()];
            self.vector_info[reuse_index.index()] = thread;
            self.threads.vector_index[old_thread.index()] = None;
            reuse_index
        } else {
            self.vector_info.push(thread);
            VectorIdx((self.vector_info.len() - 1) as u32)
        };
        self.threads.vector_index[thread.index()] = Some(created_index);
        if created_index.index() == self.vector_clocks.len() {
            self.vector_clocks.push(ThreadClockSet::default());
        }

        let (current, created) =
            pick2_mut(&mut self.vector_clocks, current_index.index(), created_index.index());
        created.join_with(current);
        // Both count as releases.
        current.increment_clock(current_index, site);
        created.increment_clock(created_index, site);
    }

    /// The running thread's clock, given to `dst`; the thread then moves on, so nothing it does
    /// later is confused with what it released (Miri: `release_clock`).
    fn release_into(&mut self, dst: &mut VClock, site: SiteId) {
        let index = self.active_index();
        let clocks = &mut self.vector_clocks[index.index()];
        dst.clone_from(&clocks.clock);
        clocks.increment_clock(index, site);
    }

    /// Miri: `acquire_clock`. An acquire doesn't move the thread's time.
    fn acquire(&mut self, clock: &VClock) {
        let index = self.active_index();
        self.vector_clocks[index.index()].clock.join(clock);
    }

    /// The running thread exits: its final clock is kept for a join, and its index may be
    /// reused (Miri: `thread_terminated`).
    fn thread_terminated(&mut self) {
        let current_thread = self.active;
        let current_index = self.active_index();
        let mut termination_clock = VClock::default();
        self.release_into(&mut termination_clock, SiteId::START);
        self.threads.termination_clock[current_thread.index()] = Some(termination_clock);
        if !self.reuse_candidates.contains(&current_index) {
            self.reuse_candidates.push(current_index);
        }
    }

    /// The running thread joined `joinee`, which exited: it acquires the final clock. If no
    /// other thread is live and every thread's time is within `joinee`'s clock, races are
    /// impossible until the next spawn (Miri: `thread_joined`, which compares with the joinee's
    /// clock, not the joiner's; that rarely holds, so detection mostly stays on).
    fn thread_joined(&mut self, joinee: ThreadId) {
        let index = self.active_index();
        let join_clock = self.threads.termination_clock[joinee.index()]
            .as_ref()
            .expect("joined with thread but thread has not terminated");
        self.vector_clocks[index.index()].clock.join(join_clock);

        if let Some(current_index) = self.threads.vector_index[joinee.index()]
            && self.threads.live_count() == 1
        {
            let current_clock = &self.vector_clocks[current_index.index()];
            if self.vector_clocks.iter().enumerate().all(|(idx, clocks)| {
                let idx = VectorIdx(idx as u32);
                clocks.clock[idx] <= current_clock.clock[idx]
            }) {
                self.multi_threaded = false;
            }
        }
    }

    /// Miri: `atomic_fence`. AcqRel and SeqCst are an acquire and a release fence at one time;
    /// an SC fence is also an RMW of the global fence location.
    fn fence(&mut self, ord: AtomicOrd, site: SiteId) {
        if !self.multi_threaded || ord == AtomicOrd::Relaxed {
            return;
        }
        let index = self.active_index();
        let clocks = &mut self.vector_clocks[index.index()];
        if ord != AtomicOrd::Release {
            clocks.apply_acquire_fence();
        }
        if ord == AtomicOrd::SeqCst {
            // This rules out legal behaviors, but no more precise model is known
            // (<https://www.cs.tau.ac.il/~orilahav/papers/popl21_robustness.pdf>, page 10).
            self.last_sc_fence.join(&clocks.clock);
            clocks.clock.join(&self.last_sc_fence);
            // Only respected by later reads.
            clocks.write_seqcst.join(&self.last_sc_write_per_thread);
        }
        // Last, since the above may grow the clock it releases.
        if ord != AtomicOrd::Acquire {
            clocks.apply_release_fence();
            clocks.increment_clock(index, site);
        }
    }

    /// The SC ATOMIC STORE rule of the paper (Miri: `sc_write`).
    fn sc_write(&mut self) {
        let index = self.active_index();
        self.last_sc_write_per_thread.set_at_index(&self.vector_clocks[index.index()].clock, index);
    }

    /// The SC ATOMIC READ rule of the paper (Miri: `sc_read`).
    fn sc_read(&mut self) {
        let index = self.active_index();
        self.vector_clocks[index.index()].read_seqcst.join(&self.last_sc_fence);
    }

    /// A non-atomic access of `range` of `alloc`'s `cells` (Miri: `VClockAlloc::read_non_atomic`
    /// and `write_non_atomic`).
    fn na_access(
        &mut self,
        cells: &mut RangeMap<MemoryCellClocks>,
        alloc: AllocId,
        range: Span,
        access: AccessType,
        site: SiteId,
    ) -> Result<(), RaceError> {
        let index = self.active_index();
        let clocks = &mut self.vector_clocks[index.index()];
        for (cell_range, cell) in cells.iter_mut(range) {
            let res = match access {
                AccessType::NaRead(read_type) => cell.non_atomic_read_detect(clocks, index, read_type, site),
                AccessType::NaWrite(write_type) => {
                    cell.non_atomic_write_detect(clocks, index, write_type, site)
                }
                AccessType::AtomicLoad | AccessType::AtomicStore | AccessType::AtomicRmw => {
                    unreachable!("an atomic access through na_access")
                }
            };
            if res.is_err() {
                return Err(self.report(cell, access, range.len, alloc, cell_range.start, site));
            }
        }
        Ok(())
    }

    /// An atomic access, `op` on each cell; the thread's time moves on after it, as if every
    /// atomic released (Miri: `validate_atomic_op` and `maybe_perform_sync_operation`).
    ///
    /// The thread's timestamp takes `site` before `op` runs, so the clocks an atomic access
    /// leaves in a cell name it; Miri names the operation that last moved the thread's time.
    fn atomic_op(
        &mut self,
        cells: &mut RangeMap<MemoryCellClocks>,
        alloc: AllocId,
        range: Span,
        access: AccessType,
        site: SiteId,
        mut op: impl FnMut(&mut MemoryCellClocks, &mut ThreadClockSet, VectorIdx) -> Result<(), cells::DataRace>,
    ) -> Result<(), RaceError> {
        debug_assert!(access.is_atomic());
        if !self.multi_threaded {
            return Ok(());
        }
        let index = self.active_index();
        let clocks = &mut self.vector_clocks[index.index()];
        if !is_dummy(site) {
            clocks.clock.index_mut(index).site = site;
        }
        for (cell_range, cell) in cells.iter_mut(range) {
            if op(cell, clocks, index).is_err() {
                return Err(self.report(cell, access, range.len, alloc, cell_range.start, site));
            }
        }
        self.vector_clocks[index.index()].increment_clock(index, site);
        Ok(())
    }

    /// The race `cell` found with `access`: which earlier access, on which thread (Miri:
    /// `VClockAlloc::report_data_race`).
    #[cold]
    #[inline(never)]
    fn report(
        &self,
        cell: &MemoryCellClocks,
        access: AccessType,
        access_size: u64,
        alloc: AllocId,
        offset: u64,
        site: SiteId,
    ) -> RaceError {
        let active_index = self.active_index();
        let active_clocks = &self.vector_clocks[active_index.index()];
        // `Some`: a race of mixed sizes.
        let mut other_size = None;
        let write_clock;
        let (other_access, other_index, other_clock): (AccessType, VectorIdx, &VClock) =
            if !access.is_atomic()
                && let Some(atomic) = cell.atomic()
                && let Some(idx) = atomic.write_vector.find_gt_index(&active_clocks.clock)
            {
                (AccessType::AtomicStore, idx, &atomic.write_vector)
            } else if !access.is_atomic()
                && !access.is_read()
                && let Some(atomic) = cell.atomic()
                && let Some(idx) = atomic.read_vector.find_gt_index(&active_clocks.clock)
            {
                (AccessType::AtomicLoad, idx, &atomic.read_vector)
            } else if cell.write.1 > active_clocks.clock[cell.write.0] {
                write_clock = cell.write();
                (AccessType::NaWrite(cell.write_type), cell.write.0, &write_clock)
            } else if !access.is_read()
                && let Some(idx) = cell.read.find_gt_index(&active_clocks.clock)
            {
                (AccessType::NaRead(cell.read[idx].read_type()), idx, &cell.read)
            } else if access.is_atomic()
                && let Some(atomic) = cell.atomic()
                && atomic.size != Some(access_size)
            {
                // A race only if some atomic access is unsynchronized: find it.
                other_size = Some(atomic.size.unwrap_or(0));
                if let Some(idx) = atomic.write_vector.find_gt_index(&active_clocks.clock) {
                    (AccessType::AtomicStore, idx, &atomic.write_vector)
                } else if let Some(idx) = atomic.read_vector.find_gt_index(&active_clocks.clock) {
                    (AccessType::AtomicLoad, idx, &atomic.read_vector)
                } else {
                    unreachable!("Failed to report data-race for mixed-size access: no race found")
                }
            } else {
                unreachable!("Failed to report data-race")
            };

        let involves_non_atomic = !access.is_atomic() || !other_access.is_atomic();
        assert!(
            other_size.is_some() || !(access.is_read() && other_access.is_read()),
            "there should be no same-size read-read races\naccess: {access:?}\nother_access: {other_access:?}"
        );
        let other_thread = self.vector_info[other_index.index()];
        let active_thread = self.vector_info[active_index.index()];
        RaceError::new(RaceErrorKind::Race(DataRace {
            involves_non_atomic,
            alloc,
            offset,
            op1: RacingOp {
                action: other_access.action(),
                size: other_size,
                thread: other_thread,
                thread_name: self.threads.display_name(other_thread),
                site: other_clock[other_index].site,
            },
            op2: RacingOp {
                action: access.action(),
                size: other_size.map(|_| access_size),
                thread: active_thread,
                thread_name: self.threads.display_name(active_thread),
                site,
            },
            mixed_size: other_size.is_some(),
            retag: access.is_retag() || other_access.is_retag(),
        }))
    }
}

/// The race detector's state of one allocation.
#[derive(Debug)]
struct AllocClocks {
    cells: RangeMap<MemoryCellClocks>,
    /// Empty unless weak memory is on.
    buffers: StoreBuffers,
}

impl AllocClocks {
    fn new(size: u64, timestamp: VTimestamp, index: VectorIdx) -> AllocClocks {
        AllocClocks {
            cells: RangeMap::new(size, MemoryCellClocks::new(timestamp, index)),
            buffers: StoreBuffers::default(),
        }
    }
}

#[derive(Debug)]
enum Slot {
    /// Never made, or freed.
    Absent,
    /// Made while nothing could race and not accessed since threads exist.
    Lazy { size: u64 },
    Live(AllocClocks),
}

/// The clocks of `alloc`, made now if it was [`Slot::Lazy`]; `None` if it is not live.
fn live(allocs: &mut [Slot], alloc: AllocId) -> Option<&mut AllocClocks> {
    let slot = allocs.get_mut(alloc.index())?;
    if let Slot::Lazy { size } = *slot {
        // Everything done to it so far happens before every thread's current time.
        *slot = Slot::Live(AllocClocks::new(size, VTimestamp::ZERO, VectorIdx(0)));
    }
    match slot {
        Slot::Live(clocks) => Some(clocks),
        Slot::Absent | Slot::Lazy { .. } => None,
    }
}

/// What the last atomic load read.
#[derive(Debug)]
enum Loaded {
    /// The latest store: the bytes in memory.
    Latest,
    /// An older store's bytes; `None` if they were not initialized.
    Outdated(Option<Value>),
}

#[derive(Debug)]
struct PendingLoad {
    alloc: AllocId,
    range: Span,
    value: Loaded,
}

/// The acquire or relaxed half of an atomic load of `size` bytes, reading the store whose clock
/// is `sync_clock` (`None`: the latest).
fn load_op(
    acquire: bool,
    size: u64,
    sync_clock: Option<&VClock>,
) -> impl FnMut(&mut MemoryCellClocks, &mut ThreadClockSet, VectorIdx) -> Result<(), cells::DataRace> + '_ {
    move |cell: &mut MemoryCellClocks, clocks: &mut ThreadClockSet, index: VectorIdx| {
        if acquire {
            cell.load_acquire(clocks, index, size, sync_clock)
        } else {
            cell.load_relaxed(clocks, index, size, sync_clock)
        }
    }
}

/// Threads, their clocks, and the clocks of every allocation of one run.
#[derive(Debug)]
pub struct Races {
    preemption_rate: f64,
    weak_memory: bool,
    rng: SplitMix64,
    /// The running thread asked to give way (Miri: `ThreadManager::yield_active_thread`).
    yield_requested: bool,
    global: Global,
    /// By [`AllocId::index`].
    allocs: Vec<Slot>,
    /// The clock of each synchronization object by address (Miri: the `clock` of `Mutex`,
    /// `Futex`, `Condvar` and `InitOnce` in `sync.rs` and `init_once.rs`).
    sync: HashMap<u64, VClock>,
    /// Set by an atomic [`Races::read`], for [`Races::load_value`].
    pending_load: Option<PendingLoad>,
    /// Set by [`Races::rmw`], for [`Races::store_value`].
    pending_rmw: Option<(AllocId, Span)>,
}

impl Races {
    /// A run with the main thread, [`ThreadId::MAIN`], running.
    pub fn new(config: &Config) -> Races {
        Races {
            preemption_rate: config.preemption_rate,
            weak_memory: config.weak_memory,
            rng: SplitMix64::new(config.seed),
            yield_requested: false,
            global: Global::new(),
            allocs: Vec::new(),
            sync: HashMap::default(),
            pending_load: None,
            pending_rmw: None,
        }
    }

    /// The running thread.
    pub fn active(&self) -> ThreadId {
        self.global.active
    }

    /// Whether every thread, `main` too, has exited (Miri: `have_all_terminated`). At the end of
    /// a run, after `main`'s [`Races::exit`], `false` is Miri's leak error "the main thread
    /// terminated without waiting for all remaining threads".
    pub fn all_terminated(&self) -> bool {
        self.global.threads.state.iter().all(|&s| s == ThreadState::Terminated)
    }

    /// The name `thread` was given, if any; `main` is named "main" (Miri: `get_thread_name`).
    pub fn thread_name(&self, thread: ThreadId) -> Option<&str> {
        self.global.threads.name[thread.index()].as_deref()
    }

    /// The name reports use: the given one, or `unnamed-N` (Miri: `get_thread_display_name`).
    pub fn thread_display_name(&self, thread: ThreadId) -> String {
        self.global.threads.display_name(thread)
    }

    /// A new thread, made by the running one, which goes on running. Everything the parent did
    /// happens before the child's first step.
    pub fn spawn(&mut self) -> ThreadId {
        let thread = self.global.threads.push(None);
        self.global.thread_created(thread, SiteId::START);
        thread
    }

    /// Gives `thread` a name for reports (`pthread_setname_np`, `thread::Builder::name`).
    pub fn set_thread_name(&mut self, thread: ThreadId, name: &[u8]) {
        self.global.threads.name[thread.index()] = Some(String::from_utf8_lossy(name).into());
    }

    /// The running thread ends (Miri: `terminate_active_thread`). Call [`Races::schedule`]
    /// next, without it among the runnable.
    pub fn exit(&mut self) {
        self.pending_load = None;
        self.pending_rmw = None;
        let thread = self.global.active;
        self.global.threads.state[thread.index()] = ThreadState::Terminated;
        self.global.thread_terminated();
    }

    /// The running thread joins `joined`, which has exited: it acquires `joined`'s final
    /// clock (Miri: `join_thread_exclusive` and `join_thread`).
    pub fn join(&mut self, joined: ThreadId) -> Result<(), RaceError> {
        let status = self.global.threads.join[joined.index()];
        if status == JoinStatus::Joined {
            return Err(RaceError::new(RaceErrorKind::JoinJoined(joined)));
        }
        if joined == self.global.active {
            return Err(RaceError::new(RaceErrorKind::JoinSelf(joined)));
        }
        if status == JoinStatus::Detached {
            return Err(RaceError::new(RaceErrorKind::JoinDetached(joined)));
        }
        if self.global.threads.state[joined.index()] != ThreadState::Terminated {
            return Err(RaceError::new(RaceErrorKind::JoinRunning(joined)));
        }
        self.global.threads.join[joined.index()] = JoinStatus::Joined;
        self.global.thread_joined(joined);
        Ok(())
    }

    /// `thread`'s handle is dropped: nobody may join it (Miri: `detach_thread`).
    pub fn detach(&mut self, thread: ThreadId) -> Result<(), RaceError> {
        if self.global.threads.join[thread.index()] != JoinStatus::Joinable {
            return Err(RaceError::new(RaceErrorKind::DetachTwice(thread)));
        }
        self.global.threads.join[thread.index()] = JoinStatus::Detached;
        Ok(())
    }

    /// The running thread gives way at the next [`Races::schedule`] (Miri:
    /// `yield_active_thread`).
    pub fn yield_active(&mut self) {
        self.yield_requested = true;
    }

    /// The thread to run next (Miri: `scheduler.rs`, `schedule`, with its preemption point
    /// `maybe_preempt_active_thread` at every terminator): `running` goes on while it is
    /// runnable unless it yielded or is preempted, at `Config::preemption_rate`; otherwise the
    /// next runnable thread after it in id order, round robin, so a yield moves on whenever
    /// another thread can run. `runnable` holds every thread that is neither blocked nor ended.
    pub fn schedule(&mut self, running: ThreadId, runnable: &[ThreadId]) -> Result<ThreadId, Deadlock> {
        if runnable.is_empty() {
            let threads = (0..self.global.threads.state.len() as u32)
                .map(ThreadId)
                .filter(|t| self.global.threads.state[t.index()] != ThreadState::Terminated)
                .collect();
            return Err(Deadlock { threads });
        }
        let can_continue = runnable.contains(&running);
        if can_continue && !self.yield_requested && self.rng.chance(self.preemption_rate) {
            self.yield_requested = true;
        }
        if can_continue && !self.yield_requested {
            self.global.active = running;
            return Ok(running);
        }
        // Threads after `running`, then those before it, then `running` itself.
        let mut after = None;
        let mut first = runnable[0];
        for &t in runnable {
            if t > running && after.is_none_or(|a| t < a) {
                after = Some(t);
            }
            if t < first {
                first = t;
            }
        }
        let next = after.unwrap_or(first);
        self.yield_requested = false;
        self.global.active = next;
        Ok(next)
    }

    /// Whether a `compare_exchange_weak` that would succeed fails spuriously.
    pub fn spurious_failure(&mut self) -> bool {
        self.rng.chance(CMPXCHG_WEAK_FAILURE_RATE)
    }

    /// A new allocation of `size` bytes (Miri: `VClockAlloc::new_allocation`). `stack_of` is the
    /// thread that made it (its stack, or the heap through it), counted as writing every byte
    /// now; `None` is memory that exists before every thread (statics, constants, the
    /// evaluator's own), written at time zero.
    pub fn new_alloc(&mut self, alloc: AllocId, size: u64, stack_of: Option<ThreadId>) {
        let i = alloc.index();
        if self.allocs.len() <= i {
            self.allocs.resize_with(i + 1, || Slot::Absent);
        }
        self.allocs[i] = if !self.global.multi_threaded {
            Slot::Lazy { size }
        } else {
            let (timestamp, index) = match stack_of {
                Some(thread) => {
                    let index = self.global.thread_index(thread);
                    (self.global.vector_clocks[index.index()].clock[index], index)
                }
                None => (VTimestamp::ZERO, self.global.thread_index(ThreadId::MAIN)),
            };
            Slot::Live(AllocClocks::new(size, timestamp, index))
        };
    }

    /// `alloc`, `size` bytes of memory from before every thread, has its initial bytes, which
    /// the evaluator wrote when the program first used it: they are no thread's write, so its
    /// clocks start again at time zero.
    pub fn initialized(&mut self, alloc: AllocId, size: u64) {
        self.new_alloc(alloc, size, None);
    }

    /// `alloc`, `size` bytes, is freed: a write of every byte. Its clocks go with it; memory
    /// reports any later access as use after free.
    pub fn free(&mut self, alloc: AllocId, size: u64, site: SiteId) -> Result<(), RaceError> {
        self.pending_load = None;
        self.pending_rmw = None;
        let Some(slot) = self.allocs.get_mut(alloc.index()) else { return Ok(()) };
        let result = match slot {
            Slot::Live(clocks) if self.global.multi_threaded && size > 0 => self.global.na_access(
                &mut clocks.cells,
                alloc,
                Span::new(0, size),
                AccessType::NaWrite(NaWriteType::Deallocate),
                site,
            ),
            // A lazy allocation was never accessed while threads existed: nothing to race with.
            Slot::Live(_) | Slot::Lazy { .. } | Slot::Absent => Ok(()),
        };
        *slot = Slot::Absent;
        result
    }

    /// A read of `range` of `alloc`: non-atomic (`None`), or an atomic load with its ordering.
    /// An atomic load also picks, under weak memory, the store it reads; [`Races::load_value`]
    /// gives its bytes.
    pub fn read(
        &mut self,
        alloc: AllocId,
        range: Span,
        atomic: Option<AtomicOrd>,
        site: SiteId,
    ) -> Result<(), RaceError> {
        self.pending_rmw = None;
        self.pending_load = None;
        match atomic {
            None => self.na(alloc, range, AccessType::NaRead(NaReadType::Read), site),
            Some(ord) => self.atomic_load(alloc, range, ord, site),
        }
    }

    /// A write of `range` of `alloc`: non-atomic (`None`), or an atomic store with its ordering,
    /// whose bytes [`Races::store_value`] gets next.
    pub fn write(
        &mut self,
        alloc: AllocId,
        range: Span,
        atomic: Option<AtomicOrd>,
        site: SiteId,
    ) -> Result<(), RaceError> {
        self.pending_rmw = None;
        self.pending_load = None;
        match atomic {
            None => {
                self.na(alloc, range, AccessType::NaWrite(NaWriteType::Write), site)?;
                if self.weak_memory
                    && let Some(Slot::Live(clocks)) = self.allocs.get_mut(alloc.index())
                {
                    clocks.buffers.non_atomic_write(range);
                }
                Ok(())
            }
            Some(ord) => {
                let size = range.len;
                let release = ord.releases();
                self.atomic(alloc, range, AccessType::AtomicStore, site, move |cell, clocks, index| {
                    if release {
                        cell.store_release(clocks, index, size)
                    } else {
                        cell.store_relaxed(clocks, index, size)
                    }
                })
            }
        }
    }

    /// An atomic read-modify-write of `range` of `alloc` that succeeded (a compare-exchange that
    /// failed is [`Races::failed_cxchg`]): it reads the latest store, and its bytes go to
    /// [`Races::store_value`] next (Miri: `validate_atomic_rmw`).
    pub fn rmw(&mut self, alloc: AllocId, range: Span, ord: AtomicOrd, site: SiteId) -> Result<(), RaceError> {
        self.pending_load = None;
        self.pending_rmw = None;
        let size = range.len;
        let acquire = ord.acquires();
        let release = ord.releases();
        self.atomic(alloc, range, AccessType::AtomicRmw, site, move |cell, clocks, index| {
            if acquire {
                cell.load_acquire(clocks, index, size, None)?;
            } else {
                cell.load_relaxed(clocks, index, size, None)?;
            }
            if release {
                cell.rmw_release(clocks, index, size)
            } else {
                cell.rmw_relaxed(clocks, index, size)
            }
        })?;
        self.pending_rmw = Some((alloc, range));
        Ok(())
    }

    /// A compare-exchange that failed: a load with the failure ordering of the latest store
    /// (Miri: `atomic_compare_exchange_scalar` and `perform_read_on_buffered_latest`).
    pub fn failed_cxchg(
        &mut self,
        alloc: AllocId,
        range: Span,
        ord: AtomicOrd,
        site: SiteId,
    ) -> Result<(), RaceError> {
        self.pending_load = None;
        self.pending_rmw = None;
        self.atomic(alloc, range, AccessType::AtomicLoad, site, load_op(ord.acquires(), range.len, None))?;
        if self.weak_memory {
            let is_seqcst = ord == AtomicOrd::SeqCst;
            if is_seqcst {
                self.global.sc_read();
            }
            if !range.is_empty()
                && let Some(Slot::Live(clocks)) = self.allocs.get_mut(alloc.index())
                && let Some(pos) = clocks.buffers.find(range)
            {
                let index = self.global.active_index();
                clocks.buffers.get_mut(pos).read_from_last_store(
                    index,
                    &self.global.vector_clocks[index.index()],
                    is_seqcst,
                );
            }
        }
        Ok(())
    }

    /// The implicit read or write of a retag of `range` of `alloc` (Miri: Tree Borrows'
    /// `reborrow`, which tells the race detector of the access it does).
    pub fn retag(&mut self, alloc: AllocId, range: Span, kind: AccessKind, site: SiteId) -> Result<(), RaceError> {
        let access = match kind {
            AccessKind::Read => AccessType::NaRead(NaReadType::Retag),
            AccessKind::Write => AccessType::NaWrite(NaWriteType::Retag),
        };
        self.na(alloc, range, access, site)
    }

    /// The bytes the atomic load [`Races::read`] just did of `range` of `alloc` reads, when weak
    /// memory picked a store older than the latest; `None`: the bytes in memory.
    pub fn load_value(&self, alloc: AllocId, range: Span) -> Result<Option<&[u8]>, RaceError> {
        let Some(pending) = &self.pending_load else {
            panic!("load_value of {alloc:?} {range:?} with no atomic read of it before")
        };
        assert!(
            pending.alloc == alloc && pending.range == range,
            "load_value of {alloc:?} {range:?} after an atomic read of {:?} {:?}",
            pending.alloc,
            pending.range
        );
        match &pending.value {
            Loaded::Latest => Ok(None),
            Loaded::Outdated(Some(value)) => Ok(Some(value.as_slice())),
            Loaded::Outdated(None) => {
                Err(RaceError::new(RaceErrorKind::UninitLoad { alloc, offset: range.start }))
            }
        }
    }

    /// The bytes of the atomic store or RMW just checked by [`Races::write`] or [`Races::rmw`],
    /// for later weak loads (Miri: `buffered_atomic_write` and `buffered_atomic_rmw`). `old` is
    /// what the location held before (`None`: not all initialized): a location's first atomic
    /// store keeps it as the store before it.
    pub fn store_value(&mut self, alloc: AllocId, range: Span, ord: AtomicOrd, bytes: &[u8], old: Option<&[u8]>) {
        let is_rmw = self.pending_rmw.take() == Some((alloc, range));
        self.pending_load = None;
        if !self.weak_memory || range.is_empty() {
            return;
        }
        let is_seqcst = ord == AtomicOrd::SeqCst;
        if is_seqcst {
            if is_rmw {
                self.global.sc_read();
            }
            self.global.sc_write();
        }
        let clocks = if self.global.multi_threaded {
            live(&mut self.allocs, alloc)
        } else {
            // A lazy allocation needs no store buffer while nothing can race: every store to it
            // happens before every later thread, which so reads only the latest.
            match self.allocs.get_mut(alloc.index()) {
                Some(Slot::Live(clocks)) => Some(clocks),
                Some(Slot::Lazy { .. } | Slot::Absent) | None => None,
            }
        };
        let Some(AllocClocks { cells, buffers }) = clocks else { return };

        let mut sync_clock = VClock::default();
        for (_, cell) in cells.iter(range) {
            if let Some(atomic) = cell.atomic() {
                sync_clock.join(&atomic.sync_vector);
            }
        }
        let pos = buffers.find_or_create(range, old);
        let index = self.global.active_index();
        let thread_clocks = &self.global.vector_clocks[index.index()];
        let buffer = buffers.get_mut(pos);
        if is_rmw {
            buffer.read_from_last_store(index, thread_clocks, is_seqcst);
        }
        buffer.store(Some(bytes), index, &thread_clocks.clock, is_seqcst, sync_clock);
    }

    /// An atomic fence. `Relaxed` is no fence and does nothing.
    pub fn fence(&mut self, ord: AtomicOrd) {
        self.global.fence(ord, SiteId::START);
    }

    /// The running thread releases into the synchronization object at `object` (a futex word,
    /// a lock): a later [`Races::acquire`] of it happens after this (Miri: `futex_wake`,
    /// `mutex_unlock`, `condvar_signal`, `init_once_complete`).
    pub fn release(&mut self, object: u64) {
        let clock = self.sync.entry(object).or_default();
        self.global.release_into(clock, SiteId::START);
    }

    /// The running thread acquires the synchronization object at `object` (Miri: `futex_wait`'s
    /// wake-up, `mutex_lock`, `condvar_wait`, `init_once_observe_completed`).
    pub fn acquire(&mut self, object: u64) {
        if let Some(clock) = self.sync.get(&object) {
            self.global.acquire(clock);
        }
    }

    /// A non-atomic access, when races are possible.
    fn na(&mut self, alloc: AllocId, range: Span, access: AccessType, site: SiteId) -> Result<(), RaceError> {
        if !self.global.multi_threaded || range.is_empty() {
            return Ok(());
        }
        let Some(clocks) = live(&mut self.allocs, alloc) else { return Ok(()) };
        self.global.na_access(&mut clocks.cells, alloc, range, access, site)
    }

    /// An atomic access with the clock changes `op` makes, when races are possible.
    fn atomic(
        &mut self,
        alloc: AllocId,
        range: Span,
        access: AccessType,
        site: SiteId,
        op: impl FnMut(&mut MemoryCellClocks, &mut ThreadClockSet, VectorIdx) -> Result<(), cells::DataRace>,
    ) -> Result<(), RaceError> {
        if !self.global.multi_threaded || range.is_empty() {
            return Ok(());
        }
        let Some(clocks) = live(&mut self.allocs, alloc) else { return Ok(()) };
        self.global.atomic_op(&mut clocks.cells, alloc, range, access, site, op)
    }

    /// An atomic load (Miri: `read_scalar_atomic` and `buffered_atomic_read`): under weak memory,
    /// with a store buffer for exactly `range`, pick the store it reads first and acquire that
    /// store's clock; otherwise read the latest.
    fn atomic_load(&mut self, alloc: AllocId, range: Span, ord: AtomicOrd, site: SiteId) -> Result<(), RaceError> {
        let size = range.len;
        let acquire = ord.acquires();
        let is_seqcst = ord == AtomicOrd::SeqCst;
        if self.weak_memory {
            if is_seqcst {
                self.global.sc_read();
            }
            if !range.is_empty()
                && let Some(Slot::Live(clocks)) = self.allocs.get_mut(alloc.index())
                && let Some(pos) = clocks.buffers.find(range)
            {
                let AllocClocks { cells, buffers } = clocks;
                let buffer = buffers.get_mut(pos);
                let index = self.global.active_index();
                // The store is picked with the clocks before this load's own acquire, as in the
                // paper's ATOMIC LOAD rule.
                let (chosen, latest) =
                    buffer.fetch_store(is_seqcst, &self.global.vector_clocks[index.index()], &mut self.rng);
                let sync_clock = &buffer.element(chosen).sync_clock;
                self.global.atomic_op(cells, alloc, range, AccessType::AtomicLoad, site, load_op(acquire, size, Some(sync_clock)))?;
                let index = self.global.active_index();
                let elem = buffer.load(chosen, index, &self.global.vector_clocks[index.index()], is_seqcst);
                let value = if latest { Loaded::Latest } else { Loaded::Outdated(elem.val.clone()) };
                self.pending_load = Some(PendingLoad { alloc, range, value });
                return Ok(());
            }
        }
        self.atomic(alloc, range, AccessType::AtomicLoad, site, load_op(acquire, size, None))?;
        self.pending_load = Some(PendingLoad { alloc, range, value: Loaded::Latest });
        Ok(())
    }

    #[cfg(test)]
    fn buffer_count(&self, alloc: AllocId) -> usize {
        match self.allocs.get(alloc.index()) {
            Some(Slot::Live(clocks)) => clocks.buffers.count(),
            _ => 0,
        }
    }

    #[cfg(test)]
    fn is_multi_threaded(&self) -> bool {
        self.global.multi_threaded
    }

    #[cfg(test)]
    fn is_lazy(&self, alloc: AllocId) -> bool {
        matches!(self.allocs.get(alloc.index()), Some(Slot::Lazy { .. }))
    }
}
