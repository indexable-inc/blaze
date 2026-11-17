//! The ring a batch's tasks end into (std.md, "End state: efficiency", piece 15: "a ring per
//! scope replaces the count's sweep, behind `Joining`").
//!
//! **What it replaces.** A batch join found its ended tasks by sweeping every open one
//! (`Joining::done` over each position), amortized by a share of the open count. That costs the
//! open tasks per sweep and reads every task's claim, a cache line each.
//!
//! **The ring.** A batch is given its capacity at its start (`Scope::batch`): the most tasks it
//! will spawn. It allocates that many slots once. Each of its tasks, as it ends, takes the next
//! slot with one `fetch_add` on the ring's cursor and stores its own job's header there
//! ([`end_task`]). The joiner reads the slots in order from its own position, delivering each
//! one it finds written and stopping at the first that is not yet ([`EndRing::next`]). So
//! delivery costs the tasks that ended, never the ones still open, and no lock is taken. Each
//! task pushes once and the batch refuses a spawn past its capacity, so the cursor never passes
//! the end: the ring never wraps, and no slot is written twice.
//!
//! **No wake is lost** ([`end_task`]'s order): the slot is written (`Release`), then the task is
//! marked done, then the scope's count of ended tasks moves and its thread is unparked. The
//! joiner reads that count BEFORE it reads the slots and parks only while the count still has
//! the value it read (task/help.rs `drive`). It takes a slot only once its task is done
//! ([`EndRing::next`]'s `done`): a slot written by a task not yet done stops the read there,
//! and that task's count has not moved yet, so the park waits for it. A task that ended before
//! the read is in its slot and done; one that ends after it moves the count, so the park does
//! not wait for it. The slot is written before `DONE`, never after, so a batch that has seen
//! every task done has no write into its ring still to come. The control
//! [`PoolPlant::WakeBeforeRingPush`] moves the count and wakes before the slot is written; the
//! model (below, `model_pool_*`) finds the joiner asleep with a result in its slot.
//!
//! **Which tasks.** Only the job made inside a batch's own spawn carries the ring: the batch
//! installs its ring's end on this thread for the length of one spawn ([`install`]), and the job
//! made then takes it ([`take_install`]); a job made later (by a task run inline during that
//! spawn, or by any other spawn of the scope) finds none and ends into no ring.

#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
#[cfg(not(blaze_check))]
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use core::{alloc::Allocator, cell::Cell, fmt, ptr::NonNull};

use blaze_own::Vec;

use super::threads::Header;

/// A planted bug for the pool model's controls (rule 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PoolPlant {
    None,
    /// The ending task moves the scope's count and wakes the joiner before it writes its slot:
    /// the joiner reads the moved count, finds the slot empty and parks for a count that never
    /// moves again, with the result ready.
    #[cfg_attr(not(blaze_check), expect(dead_code, reason = "the model's control"))]
    WakeBeforeRingPush,
}

/// Why a batch could not make its ring: its allocator refused (the batch names its scope).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct RingNoRoom;

/// A task ended into a ring past its capacity: a batch that spawned more than it said at its
/// start, which [`crate::task::Batch::may_overlap`] refuses before the spawn, so this is a
/// broken invariant, named.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct RingFull {
    pub(crate) capacity: u32,
}

impl fmt::Display for RingFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::task::ring: a task ended into a batch ring past its capacity {} (the batch refuses a spawn past it; rule bounded-progress)", self.capacity)
    }
}

/// The joiner's side of a batch's ring: the slots and the cursor, each in its own allocation so
/// that moving this value moves no slot.
pub(crate) struct EndRing<H, A: Allocator> {
    /// One cell: the next slot an ending task takes.
    cursor: Vec<AtomicUsize, A>,
    /// `capacity` slots, null until their task ends.
    slots: Vec<AtomicPtr<H>, A>,
    capacity: u32,
}

impl<H, A: Allocator + Clone> EndRing<H, A> {
    /// A ring of `capacity` slots in `alloc`, all empty.
    pub(crate) fn try_new_in(capacity: u32, alloc: A) -> Result<EndRing<H, A>, RingNoRoom> {
        let refused = RingNoRoom;
        let mut cursor = Vec::new_in(alloc.clone());
        cursor.try_reserve(1).map_err(|_| refused)?;
        cursor.push(AtomicUsize::new(0));
        let mut slots = Vec::new_in(alloc);
        slots.try_reserve(capacity as usize).map_err(|_| refused)?;
        for _ in 0..capacity {
            slots.push(AtomicPtr::new(core::ptr::null_mut()));
        }
        Ok(EndRing { cursor, slots, capacity })
    }
}

impl<H, A: Allocator> EndRing<H, A> {
    /// The ending tasks' side: valid while this ring lives and is not moved out of its
    /// allocations (it never is: the buffers are made once, never grown).
    pub(crate) fn end(&self) -> RingEnd<H> {
        RingEnd {
            cursor: NonNull::from(&self.cursor[..]).cast::<AtomicUsize>(),
            slots: NonNull::from(&self.slots[..]).cast::<AtomicPtr<H>>(),
            capacity: self.capacity,
        }
    }

    /// The task that ended at position `*read`, if its slot is written AND `done` says the task
    /// it names is done: then `*read` moves on. `None` at the first slot not yet written, or
    /// written by a task not yet done: the slot is written before the task's `DONE` (so the ring
    /// is alive for the write, its batch waiting on the ring), and in both cases the count has
    /// not moved yet, so the joiner's park waits for it, and the read is tried again after.
    pub(crate) fn next(&self, read: &mut u32, done: impl Fn(NonNull<H>) -> bool) -> Option<NonNull<H>> {
        let slot = self.slots.get(*read as usize)?;
        // `Acquire`: pairs with the ending task's `Release` store of the slot.
        let found = NonNull::new(slot.load(Ordering::Acquire))?;
        // `done` reads the task's own `DONE` (`Acquire`), which orders its result before.
        if !done(found) {
            return None;
        }
        *read += 1;
        Some(found)
    }

    /// The most tasks it takes.
    pub(crate) fn capacity(&self) -> u32 {
        self.capacity
    }
}

/// An ending task's handle on its batch's ring: two pointers and the capacity, copied into the
/// task's job at its making.
pub(crate) struct RingEnd<H> {
    cursor: NonNull<AtomicUsize>,
    slots: NonNull<AtomicPtr<H>>,
    capacity: u32,
}

impl<H> Clone for RingEnd<H> {
    fn clone(&self) -> RingEnd<H> {
        *self
    }
}

impl<H> Copy for RingEnd<H> {}

// SAFETY: the pointers name atomics only, each touched through atomic operations; the ring they
// point into outlives every task that carries them (its batch finishes every task, and drops its
// handles, before the ring: `Batch`'s field order), so a second thread may hold and use a copy.
unsafe impl<H> Send for RingEnd<H> {}
// SAFETY: as for `Send`: shared use is atomic operations only.
unsafe impl<H> Sync for RingEnd<H> {}

impl<H> RingEnd<H> {
    /// Takes the next slot and writes `item` there (`Release`).
    ///
    /// # Safety
    ///
    /// The ring this came from ([`EndRing::end`]) is alive.
    pub(crate) unsafe fn push(self, item: NonNull<H>) -> Result<(), RingFull> {
        // SAFETY: the caller's contract: the cursor's allocation is alive.
        let cursor = unsafe { self.cursor.as_ref() };
        // `Relaxed`: the slot taken is this task's alone; the slot's own store publishes.
        let at = cursor.fetch_add(1, Ordering::Relaxed);
        if at >= self.capacity as usize {
            return Err(RingFull { capacity: self.capacity });
        }
        // SAFETY: `at` is below the capacity, the slots' length, and the ring is alive.
        let slot = unsafe { self.slots.add(at).as_ref() };
        slot.store(item.as_ptr(), Ordering::Release);
        Ok(())
    }
}

/// How every pool task ends (threads.rs `run` and `discard`): into its batch's ring if it has
/// one, then `finish` (its claim's `DONE`), then `count` (the scope's count of ended tasks and
/// the unpark of its thread). The order is the module's "No wake is lost".
///
/// # Safety
///
/// `ring`, when `Some`, came from a ring that is alive (its batch has not finished this task).
pub(crate) unsafe fn end_task<H>(ring: Option<RingEnd<H>>, item: NonNull<H>, plant: PoolPlant, finish: impl FnOnce(), count: impl FnOnce()) {
    if plant == PoolPlant::WakeBeforeRingPush {
        finish();
        count();
        if let Some(ring) = ring {
            // SAFETY: the caller's contract.
            if let Err(full) = unsafe { ring.push(item) } {
                panic!("{full}");
            }
        }
        return;
    }
    if let Some(ring) = ring {
        // SAFETY: the caller's contract.
        if let Err(full) = unsafe { ring.push(item) } {
            panic!("{full}");
        }
    }
    finish();
    count();
}

/// The ring a batch's spawn on this thread hands to the job that spawn makes. A `Cell` set and
/// taken on one thread around one spawn: no borrow of it is held across a run or a wait (rule
/// 27).
#[thread_local]
static INSTALL: Cell<Option<RingEnd<Header>>> = Cell::new(None);

/// The batch's side, around one spawn: the job made during it takes `ring`. Gives back what was
/// installed before (a batch spawning inside a task run inline by another batch's spawn), which
/// [`restore`] puts back after.
pub(crate) fn install(ring: RingEnd<Header>) -> Option<RingEnd<Header>> {
    INSTALL.replace(Some(ring))
}

/// After the spawn: whether its job took the ring (`false`: the task ran inline with no job, so
/// it is delivered from its handle), and the outer batch's ring put back.
pub(crate) fn restore(outer: Option<RingEnd<Header>>) -> bool {
    INSTALL.replace(outer).is_none()
}

/// The job being made takes the installed ring, if any (threads.rs `job`): one job per install.
pub(crate) fn take_install() -> Option<RingEnd<Header>> {
    INSTALL.take()
}

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" cargo test -p blaze-std --release
/// model_pool`): a batch of two tasks, one worker's thread ending them through the real
/// [`end_task`] into the real [`EndRing`], while the scope's thread joins by the real
/// task/help.rs `drive`. Every task ends once and is delivered once, and the joiner never sleeps
/// with a result in its ring (the model reports a deadlock if it does).
#[cfg(all(test, blaze_check))]
mod model {
    use core::{panic::AssertUnwindSafe, ptr::NonNull};

    use blaze_check::{
        sync::atomic::{AtomicU32, AtomicU64, Ordering},
        thread,
    };
    use blaze_own::{Arc, Vec};

    use super::{EndRing, PoolPlant, end_task};
    use crate::{
        rt::catch_unwind,
        task::{
            Global,
            claim::{Claim, ClaimPlant, Start},
            help::{HelpPlant, Helped, Joining, drive},
        },
    };

    const TASKS: usize = 2;

    /// A model task: its claim and its index, where a job's header would be.
    struct Job {
        claim: Claim,
        index: u32,
        ran: AtomicU32,
    }

    struct World {
        jobs: [Job; TASKS],
        ring: EndRing<Job, Global>,
        /// `Joins::ended`.
        ended: AtomicU64,
    }

    impl World {
        /// threads.rs `run`'s end, through the real `end_task`.
        fn run(&self, at: usize, plant: PoolPlant, joiner: &thread::Thread) {
            let job = &self.jobs[at];
            job.ran.fetch_add(1, Ordering::Relaxed);
            // SAFETY: the ring lives in `self`, alive for the call.
            unsafe {
                end_task(
                    Some(self.ring.end()),
                    NonNull::from(job),
                    plant,
                    || job.claim.finish(),
                    || {
                        self.ended.fetch_add(1, Ordering::AcqRel);
                        joiner.unpark();
                    },
                );
            }
        }
    }

    /// The batch join, as help.rs `Run` does it: deliver what the ring holds, help never (the
    /// worker runs both tasks), park on the count.
    struct Sim {
        world: Arc<World, Global>,
        read: u32,
        open: usize,
        delivered: Vec<u32, Global>,
    }

    impl Joining for Sim {
        fn open(&self) -> usize {
            self.open
        }

        fn deliver_ended(&mut self) -> usize {
            let mut delivered = 0;
            // SAFETY (in the closure): a pointer into `world.jobs`, alive in the `Arc`.
            while let Some(job) = self.world.ring.next(&mut self.read, |job| unsafe { job.as_ref() }.claim.done()) {
                // SAFETY: a pointer into `world.jobs`, alive in the `Arc`.
                let job = unsafe { job.as_ref() };
                assert!(job.claim.done(), "a result was delivered before its task ended");
                self.delivered.push(job.index);
                self.open -= 1;
                delivered += 1;
            }
            delivered
        }

        fn ended(&self) -> u64 {
            self.world.ended.load(Ordering::Acquire)
        }

        fn help(&mut self) -> Helped {
            Helped::Nothing
        }

        /// `Pool::wait`'s loop on the scope's `Parker`: a token, so a wake before it is kept.
        fn park(&mut self, swept: u64) {
            while self.world.ended.load(Ordering::Acquire) == swept {
                thread::park();
            }
        }
    }

    fn batch(plant: PoolPlant) {
        let job = |index| Job { claim: Claim::new(Start::Open, ClaimPlant::None), index, ran: AtomicU32::new(0) };
        let ring = EndRing::try_new_in(u32::try_from(TASKS).expect("two tasks"), Global).expect("room for two slots");
        let world = Arc::new_in(World { jobs: [job(0), job(1)], ring, ended: AtomicU64::new(0) }, Global);
        let joiner = thread::current();
        let worker = {
            let world = Arc::clone(&world);
            thread::spawn(move || {
                for at in 0..TASKS {
                    if world.jobs[at].claim.by_worker() {
                        world.run(at, plant, &joiner);
                    }
                }
            })
        };
        let mut sim = Sim { world: Arc::clone(&world), read: 0, open: TASKS, delivered: Vec::new_in(Global) };
        drive(&mut sim, HelpPlant::None);
        worker.join().expect("the worker ends");
        for at in 0..TASKS {
            assert_eq!(world.jobs[at].ran.load(Ordering::Relaxed), 1, "a task ran other than once");
        }
        let mut seen = [0u32; TASKS];
        for index in sim.delivered.iter() {
            seen[*index as usize] += 1;
        }
        assert_eq!(seen, [1; TASKS], "an index was delivered other than once");
    }

    /// Prints
    /// `test task::ring::model::model_pool_every_end_is_delivered_once_through_the_ring ... ok`.
    #[test]
    fn model_pool_every_end_is_delivered_once_through_the_ring() {
        blaze_check::model(|| batch(PoolPlant::None));
    }

    /// The control: the count moves and the joiner is woken before the slot is written, so the
    /// joiner can read the moved count, find the ring empty and park for good: a deadlock to the
    /// model. Prints
    /// `test task::ring::model::model_pool_catches_a_wake_before_the_ring_push ... ok`.
    #[test]
    fn model_pool_catches_a_wake_before_the_ring_push() {
        let found = catch_unwind(AssertUnwindSafe(|| blaze_check::model(|| batch(PoolPlant::WakeBeforeRingPush))));
        assert!(found.is_err(), "the model must find the joiner asleep with a result in its ring");
    }
}

/// On one thread: the ring's order and its refusal past capacity.
#[cfg(all(test, not(blaze_check)))]
mod tests {
    use core::ptr::NonNull;

    use super::{EndRing, RingFull};
    use crate::task::Global;

    /// Prints `test task::ring::tests::a_ring_gives_ends_in_order_and_refuses_one_past_its_capacity ... ok`.
    #[test]
    fn a_ring_gives_ends_in_order_and_refuses_one_past_its_capacity() {
        let items = [10u32, 20, 30];
        let ring: EndRing<u32, Global> = EndRing::try_new_in(2, Global).expect("room for two slots");
        let end = ring.end();
        let mut read = 0;
        let done = |_: NonNull<u32>| true;
        assert!(ring.next(&mut read, done).is_none(), "an empty ring gave an end");
        // SAFETY: `ring` is alive.
        unsafe { end.push(NonNull::from(&items[1])) }.expect("room for the first");
        // SAFETY: as above.
        unsafe { end.push(NonNull::from(&items[0])) }.expect("room for the second");
        // SAFETY: as above.
        assert_eq!(unsafe { end.push(NonNull::from(&items[2])) }, Err(RingFull { capacity: 2 }), "a push past the capacity was taken");
        assert!(ring.next(&mut read, |_| false).is_none(), "a slot whose task is not done was given");
        assert_eq!(read, 0, "a slot whose task is not done was passed");
        let first = ring.next(&mut read, done).expect("the first end");
        let second = ring.next(&mut read, done).expect("the second end");
        // SAFETY: pointers into `items`, alive.
        assert_eq!(unsafe { (*first.as_ptr(), *second.as_ptr()) }, (20, 10), "ends are given in the order they were pushed");
        assert!(ring.next(&mut read, done).is_none(), "a ring gave more than was pushed");
        assert_eq!(ring.capacity(), 2);
    }
}
