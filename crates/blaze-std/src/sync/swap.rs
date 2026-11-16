//! [`Swap`]: readers take a consistent snapshot and never wait; a writer publishes a whole new
//! value; what it replaced is freed once no reader can hold it (std.md, "Concurrency: what to
//! reach for"). The replacement for a read-write lock.
//!
//! ```text
//! let config = Swap::new_in("bstd.config", Config::load(..), alloc);
//! // a reader, inside a task:
//! let task = reclaim::enter();
//! let now: &Config = config.read(&task);       // one acquire load; valid while `task` is
//! // a writer, anywhere:
//! config.publish(cx, Config::load(..))?;        // readers see the old or the new, whole
//! ```
//!
//! - **Readers.** [`Swap::read`] is one acquire load and a borrow. The borrow lives no longer
//!   than the reader's [`reclaim::Guard`] (the thread is in a task) and the `Swap` itself: both
//!   are lifetimes, so a snapshot that outlives either does not compile. A reader never takes a
//!   lock, never writes a line another core reads, and never holds a writer back.
//! - **Writers.** [`Swap::publish`] and [`Swap::update`] serialize on one named [`Lock`] (its
//!   wait takes `cx`), allocate the new value in the `Swap`'s allocator, publish it with one
//!   release swap, and hand the old one to the `Swap`'s limbo (`blaze_reclaim`): it is dropped
//!   at a later publish once every thread that was in a task when it was unlinked has left
//!   that task, or when the `Swap` is dropped. [`Swap::update`] reads the current value under
//!   the writers' lock, so two updates never lose one another.
//! - **Cost.** A publish is one allocation of `T`, one swap, one grace (a fence and an epoch
//!   step) and, now and then, a scan of every thread's reclamation slot, taken before the lock.
//!   A read is a load. Memory: the values not yet reclaimed, one per publish since the oldest
//!   task still running began.
//!
//! The values are `Send + Sync + 'static`: a reader on any thread borrows one, and the thread
//! that frees it is whichever publishes after its grace.

use core::{
    alloc::AllocatorClone,
    fmt,
    sync::atomic::{AtomicPtr, Ordering},
};

use blaze_own::Box;

use crate::{
    Plant, Stopped,
    cx::Cx,
    sync::{
        Lock,
        reclaim::{Grace, Guard, Horizon, Limbo},
    },
};

/// A value readers snapshot without waiting and writers replace whole. See the module's doc.
pub struct Swap<T: Send + Sync + 'static, A: AllocatorClone + Send + Sync + 'static> {
    /// The value now: a block of `T` made by `alloc` (`Box::into_raw_with_allocator`), owned by
    /// the `Swap` until a publish moves it to `retired`.
    now: AtomicPtr<T>,
    alloc: A,
    /// The replaced values, until their grace has passed; its lock is the writers'.
    retired: Lock<Limbo>,
    plant: Plant,
}

impl<T: Send + Sync + 'static, A: AllocatorClone + Send + Sync + 'static> Swap<T, A> {
    /// A `Swap` holding `value`, its values made in `alloc`, its writers' lock named `name`.
    pub fn new_in(name: &'static str, value: T, alloc: A) -> Swap<T, A> {
        Swap::planted(name, value, alloc, Plant::None)
    }

    pub(crate) fn planted(name: &'static str, value: T, alloc: A, plant: Plant) -> Swap<T, A> {
        // clone: allocators that are clones of one another free each other's blocks.
        let (now, _) = Box::into_raw_with_allocator(Box::new_in(value, alloc.clone()));
        Swap { now: AtomicPtr::new(now), alloc, retired: Lock::new(name, Limbo::new()), plant }
    }

    /// The value now, whole, for as long as the reader's task (`task`) and the `Swap` last.
    /// Never waits.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn read<'a>(&'a self, task: &'a Guard) -> &'a T {
        let _: &Guard = task;
        // Acquire: the value's bytes, written before the release swap that published it.
        let now = self.now.load(Ordering::Acquire);
        // SAFETY: `now` came from `Box::into_raw_with_allocator`, so it is a live, aligned `T`.
        // It stays live for `'a`: a publish that unlinks it hands it to the limbo under a grace
        // started after the unlink, and the limbo drops it only once every thread that was in
        // a task then has left that task. This thread is in one (`task`, held for `'a`), and
        // entered it before this load, so the grace cannot pass within `'a`. Dropping the
        // `Swap`, which frees what is left, needs it unborrowed, so not within `'a` either.
        unsafe { &*now }
    }

    /// Publishes `value` in place of the value now, which readers that already hold it keep:
    /// once the writers' lock is had, by `cx`'s deadline. The replaced value is freed after its
    /// grace (see the module's doc).
    pub fn publish(&self, cx: &Cx, value: T) -> Result<(), Stopped> {
        self.update(cx, |_| value)
    }

    /// Publishes `make(the value now)`, read under the writers' lock so no other publish comes
    /// between the read and the swap.
    pub fn update(&self, cx: &Cx, make: impl FnOnce(&T) -> T) -> Result<(), Stopped> {
        // The horizon before the lock: its scan never runs under it (blaze-reclaim, `Horizon`).
        let horizon = Horizon::now();
        let mut retired = self.retired.lock(cx)?;
        // The writers' lock is held, so `now` changes only here.
        let now = self.now.load(Ordering::Acquire);
        // SAFETY: as `read`'s; a writer holding the lock is the only one that unlinks it.
        let made = make(unsafe { &*now });
        // clone: allocators that are clones of one another free each other's blocks.
        let (made, _) = Box::into_raw_with_allocator(Box::new_in(made, self.alloc.clone()));
        // Release: `made`'s bytes before readers that load it; acquire: `old`'s, read below.
        let old = self.now.swap(made, Ordering::AcqRel);
        // SAFETY: `old` was the published block, made by a clone of `alloc` for a `T`, and the
        // swap took it out of `now`, so this is its one owner. Readers may still read it; the
        // box is not dropped until its grace has passed (the limbo), and nothing writes it.
        let old = unsafe { Box::from_raw_in(old, self.alloc.clone()) };
        if self.plant == Plant::SwapFreesAtOnce {
            drop(old);
        } else {
            // The grace starts after the swap that unlinked `old`, as `Grace::start` asks.
            retired.hold(Grace::start(), old);
        }
        let passed = retired.collect_passed(horizon);
        drop(retired);
        // Dropped outside the lock: a value's drop may be long.
        drop(passed);
        Ok(())
    }

    /// The value, through the only reference to the `Swap`: no reader can hold it.
    pub fn get_mut(&mut self) -> &mut T {
        // SAFETY: `&mut self` excludes every reader and writer; `now` is the live block.
        unsafe { &mut **self.now.get_mut() }
    }

    /// How many replaced values wait for their grace now, if the writers' lock is free.
    pub fn retired(&self) -> Option<usize> {
        self.retired.try_lock().map(|retired| retired.len())
    }
}

impl<T: Send + Sync + 'static, A: AllocatorClone + Send + Sync + 'static> Drop for Swap<T, A> {
    fn drop(&mut self) {
        // SAFETY: `&mut self`: no reader holds a borrow (`read`'s ties it to the `Swap`), so
        // the published block is this `Swap`'s alone; it was made by a clone of `alloc`.
        drop(unsafe { Box::from_raw_in(*self.now.get_mut(), self.alloc.clone()) });
        // The limbo drops what it still holds with the `Swap`'s fields: no reader can reach it.
    }
}

impl<T: Send + Sync + 'static + fmt::Debug, A: AllocatorClone + Send + Sync + 'static> fmt::Debug for Swap<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let task = crate::sync::reclaim::enter();
        f.debug_tuple("Swap").field(self.read(&task)).finish()
    }
}
