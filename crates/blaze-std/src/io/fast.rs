//! What a world reads and does without its lock (docs/v2/plan/design/std.md, "End state:
//! efficiency", pieces 6, 7 and 11): the native driver's clock, and the file calls the native
//! driver lets its asker make on the asker's own thread ([`blaze_core::threads::inline`]).
//!
//! One seam, two instantiations (rule strategy-is-a-type-parameter), chosen once as the world
//! is made ([`Beside::of`]):
//! - a native world reads its clock from a [`Monotonic`] held here, on the instant its driver
//!   began, so a read takes no lock; and makes the blocking file calls of blaze-core's table
//!   inline, its `Done` the one the queue would have delivered;
//! - a simulated world (and a guest's escape) keeps everything under its lock: its virtual
//!   clock moves only inside the driver's own calls (a poll, `idle_until`, `Sim::advance`),
//!   and every effect keeps its queue, so the simulation still reorders, fails and crashes
//!   each one. Its behaviour is the one it had before this file.
//!
//! What still waits, and its bound: a queued effect is waited for by `Cx::wait`, with its
//! deadline and its span, as before. An inline call is not cancellable: the context's deadline
//! is looked at before it, and it is one the kernel answers without waiting for another
//! process or an event (blaze-core's table says each, and its one unbounded case, a FIFO), its
//! span opened by `blaze_core::lock::io_wait`.

// Test-only: the tests' counts, shared by a world's clones.
#[cfg(test)]
use alloc::sync::Arc;
#[cfg(test)]
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::time::Duration;

use blaze_core::{
    Done, Driver, Effect, FileName, Tag,
    lock::LockGuard,
    threads::inline::{Inline, Lent, Now, inline_op},
    wait::Monotonic,
};

use super::{Backend, Ends, Io, Reach, World, change, touch};
use crate::{Stopped, cx::Cx};

/// Where a world's clock is read.
#[derive(Clone, Copy, Debug)]
pub(super) enum Time {
    /// The native driver's clock, held here: a read is the system's monotonic clock, no lock.
    Native(Monotonic),
    /// Under the world's lock: the simulation's virtual clock, which moves only inside the
    /// driver's own calls, and a guest's escape's.
    Locked,
}

/// What a shared world keeps beside its lock: what is read or done with none.
#[derive(Clone)]
pub(super) struct Beside {
    pub(super) time: Time,
    /// The native driver's inline file calls; `None` where every effect keeps its queue.
    files: Option<Inline>,
    /// The tests' view of this world (counts and the switch), shared by its clones.
    #[cfg(test)]
    pub(super) seen: Arc<Seen>,
}

/// What the tests count of one world, and their switch between the two native routes.
#[cfg(test)]
#[derive(Default)]
pub(super) struct Seen {
    /// Every reach of the world's lock through `Io::world`.
    pub(super) locks: AtomicU64,
    /// Every lent write's bytes copied into an effect the queue carries.
    pub(super) copies: AtomicU64,
    /// Every effect made inline.
    pub(super) inline: AtomicU64,
    /// The test-only switch: every effect of a native world through its queue, as before
    /// piece 7, so the two routes can be compared on one machine.
    pub(super) queued_only: AtomicBool,
}

impl Beside {
    /// What `driver` lets a world read and do without its lock.
    pub(super) fn of(driver: &Backend) -> Beside {
        let (time, files) = match driver {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, target_os = "wasi"))]
            Backend::Native(native) => (Time::Native(native.clock()), native.inline_files()),
            Backend::Sim(_) | Backend::Guest(_) => (Time::Locked, None),
        };
        Beside {
            time,
            files,
            #[cfg(test)]
            // alloc: once per world, a test's counts.
            seen: Arc::new(Seen::default()),
        }
    }

    /// The native clock's reading, or `None` for a clock under the lock.
    pub(super) fn now(&self) -> Option<Duration> {
        match self.time {
            Time::Native(clock) => Some(blaze_core::wait::Clock::now(&clock)),
            Time::Locked => None,
        }
    }
}

/// `effect` given the world's next tag, and recorded as `Io::submit` records it: the files it
/// touched and what was asked while a test records, and the generation of each file it
/// changes. Under the world's lock; the one home of this bookkeeping for both routes.
pub(super) fn stamp(world: &mut World, effect: impl FnOnce(Tag) -> Effect) -> Effect {
    let tag = Tag(world.next);
    world.next += 1;
    let effect = effect(tag);
    if world.record {
        touch(&mut world.touched, &effect);
        // clone: a test's record of what was asked.
        world.asked.push(effect.clone());
    }
    change(&mut world.changed, &effect, tag.0 + 1);
    effect
}

impl Io {
    /// What this world keeps beside its lock; `None` for a hosted run, which keeps none.
    fn kept_beside(&self) -> Option<&Beside> {
        match &self.reach {
            Reach::Shared { beside, .. } => Some(beside),
            Reach::Hosted => None,
        }
    }

    /// The native driver's inline file calls, unless the tests' switch sends everything
    /// through the queue.
    pub(super) fn inline_files(&self) -> Option<&Inline> {
        let beside = self.kept_beside()?;
        #[cfg(test)]
        if beside.seen.queued_only.load(Ordering::Relaxed) {
            return None;
        }
        beside.files.as_ref()
    }

    /// The tests' view of this world; `None` for a hosted run.
    #[cfg(test)]
    pub(super) fn seen(&self) -> Option<&Seen> {
        self.kept_beside().map(|beside| &*beside.seen)
    }

    /// [`Io::call`] on a native world with inline calls: the effect stamped under one short
    /// hold of the lock, then made on this thread if blaze-core's table says so, its `Done`
    /// the queue's; otherwise handed to the driver under that same hold and waited for as
    /// before. The context's deadline was looked at by the caller.
    pub(super) fn call_inline(&self, cx: &Cx, files: &Inline, effect: impl FnOnce(Tag) -> Effect) -> Result<Done, Stopped> {
        let mut world = self.world().lock();
        let effect = stamp(&mut world, effect);
        if inline_op(&effect).is_none() {
            let tag = effect.tag();
            self.hand_over(world, effect);
            return cx.wait(self, tag, Ends::Once);
        }
        drop(world);
        match files.perform_now(effect) {
            Now::Done(done) => {
                #[cfg(test)]
                self.counted(|seen| &seen.inline);
                Ok(done)
            }
            // The table and `perform_now` are one function's two answers; an effect handed
            // back is queued as the table says, never dropped.
            Now::Queue(effect) => {
                let tag = effect.tag();
                self.hand_over(self.world().lock(), effect);
                cx.wait(self, tag, Ends::Once)
            }
        }
    }

    /// `effect`, stamped already, handed to the driver under `world`, which this lets go: the
    /// second half of [`Io::submit`] (the simulated disk's ledger, the driver, and the power
    /// cut a `bstd::test` arms).
    pub(super) fn hand_over(&self, mut world: LockGuard<'_, World>, effect: Effect) {
        let World { driver, effects, disk, .. } = &mut *world;
        // A machine whose power `bstd::test` cut takes nothing: its ledger is told nothing.
        if let (Some(disk), Some(_)) = (disk.as_deref_mut(), driver.sim().filter(|sim| !sim.is_off())) {
            disk.submitted(&effect);
        }
        effects.push(effect);
        driver.submit(effects);
        // Under `bstd::test` the armed simulation cuts the power at an effect of its seed's: the
        // world crashes and the program's start ends here, unwound, with the lock let go first.
        let cut = driver.sim().is_some_and(blaze_core::sim::Sim::cut_due);
        drop(world);
        if cut {
            crate::simtest::power_lost(self);
        }
    }

    /// `bytes` written to `file` at `place` (its end, making it, or an offset of a file that
    /// is there), lent for the call: on a native world with inline calls, from `bytes`
    /// themselves, with no copy, the call ending once the system holds them; otherwise copied
    /// into the [`Effect::Append`] or [`Effect::WriteAt`] the queue carries, which is what the
    /// simulation reorders, fails and tears at a crash. Either way the `Done` is the effect's
    /// (`Done::Appended` or `Done::Written`), and the file's generation moves as for it.
    ///
    /// Why not a `Pending` whose op outlives the call (blaze-core's `WriteFrom` over a
    /// registered span): a wait that stops at its deadline returns while the driver's thread
    /// may still be reading the bytes, and the borrow would end under it. A lent write ends
    /// inside the call, so the borrow checker's `&[u8]` is the whole proof.
    pub(crate) fn write_lent(&self, cx: &Cx, file: &FileName, place: Lent, bytes: &[u8]) -> Result<Done, Stopped> {
        cx.stopped_by(self.now())?;
        // `bytes`: the effect's own, a `Vec<u8>` (inferred from the effect's field).
        let as_effect = |bytes, tag: Tag| {
            // clone: the effect's name, a refcount.
            let file = file.clone();
            match place {
                Lent::End => Effect::Append { file, bytes, tag },
                Lent::At(at) => Effect::WriteAt { file, at, bytes, tag },
            }
        };
        let Some(files) = self.inline_files() else {
            #[cfg(test)]
            self.counted(|seen| &seen.copies);
            // alloc: the effect's bytes, which blaze-core's `Effect` owns: the queue's route,
            // the simulation's, which keeps them to reorder and tear.
            let owned = bytes.to_vec();
            let tag = self.submit(|tag| as_effect(owned, tag));
            return cx.wait(self, tag, Ends::Once);
        };
        {
            let mut world = self.world().lock();
            // The effect this write stands for, for the generation and a test's record: its
            // bytes are copied only while a test records what was asked.
            // alloc: none unless a test records (an empty `Vec`, the `Default`, allocates nothing).
            let kept = if world.record { bytes.to_vec() } else { Default::default() };
            stamp(&mut world, |tag| as_effect(kept, tag));
        }
        let done = files.write_lent(file, place, bytes);
        #[cfg(test)]
        self.counted(|seen| &seen.inline);
        Ok(done)
    }

    /// One more of what `which` names, in this world's tests' counts.
    #[cfg(test)]
    pub(super) fn counted(&self, which: impl FnOnce(&Seen) -> &AtomicU64) {
        if let Some(seen) = self.seen() {
            which(seen).fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The tests' switch: `true` sends every effect of a native world through its queue.
    #[cfg(test)]
    pub(crate) fn queued_only(&self, on: bool) {
        if let Some(seen) = self.seen() {
            seen.queued_only.store(on, Ordering::Relaxed);
        }
    }
}
