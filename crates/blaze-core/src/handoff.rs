//! The two lock-free protocols the drivers run between their threads, kept apart from the
//! drivers so each is model-checked (docs/v2/plan/design/model-checker.md) on every platform, not
//! only where its driver runs: [`Claims`], the chunks of one job claimed by many threads and
//! completed by the last (the io_uring driver's stamp sweeps), and [`Wake`], one wake-up of a
//! driver for any number of hand-backs since it last drained them (the IOCP and kqueue drivers'
//! threads).
//! Safe code: what they promise is the memory ordering the unsafe code beside them relies on.

use crate::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(not(blaze_check))]
use crate::{
    atomic::AtomicU32,
    world::threads::{self, Ended, Step},
};

/// `chunks` pieces of one job, each claimed by exactly one thread, and the job completed by
/// exactly one: the thread whose [`Claims::finish`] counts the last chunk. Every finish is a
/// release and an acquire on one counter (a release sequence of read-modify-writes), so the
/// last finisher sees every write any claimer made to its chunk before finishing it. A claim
/// and a finish are one read-modify-write each, on two words.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "the io_uring driver's alone"))]
#[cfg_attr(
    not(target_os = "linux"),
    expect(blaze::unwired, reason = "wired on Linux; kept elsewhere for its model-checked tests")
)]
pub(crate) struct Claims {
    chunks: usize,
    /// The next chunk to claim.
    next: AtomicUsize,
    /// Chunks finished.
    finished: AtomicUsize,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "the io_uring driver's alone"))]
#[cfg_attr(
    not(target_os = "linux"),
    expect(blaze::unwired, reason = "wired on Linux; kept elsewhere for its model-checked tests")
)]
impl Claims {
    pub(crate) fn new(chunks: usize) -> Claims {
        Claims { chunks, next: AtomicUsize::new(0), finished: AtomicUsize::new(0) }
    }

    pub(crate) fn chunks(&self) -> usize {
        self.chunks
    }

    /// The next chunk, the calling thread's alone; `None` once every one is claimed. Relaxed:
    /// the counter only splits the chunks, and what a chunk's writes need ordered with is the
    /// finish. Each thread asks at most once past the end, so the counter cannot wrap.
    pub(crate) fn claim(&self) -> Option<usize> {
        let chunk = self.next.fetch_add(1, Ordering::Relaxed);
        (chunk < self.chunks).then_some(chunk)
    }

    /// The chunk the calling thread claimed is done; whether it was the last, in which case
    /// every chunk's writes happen before what the caller does next.
    pub(crate) fn finish(&self) -> bool {
        self.finished.fetch_add(1, Ordering::AcqRel) + 1 == self.chunks
    }
}

/// Hand-backs from many threads to a driver that sleeps in the kernel: each sender hands back
/// (a channel send) and then calls [`Wake::wake`], and only the first since the driver's last
/// [`Wake::drain`] is told to wake it, one system call per batch rather than per hand-back.
///
/// None is missed. Both calls are read-modify-writes of one flag, so they are totally ordered
/// and each acquires what the one before released. A sender ordered after the driver's drain
/// finds the flag clear and wakes the driver; one ordered before it is acquired by the drain,
/// so its hand-back happens before the driver reads the channel.
#[cfg_attr(
    not(any(windows, target_os = "macos")),
    allow(dead_code, reason = "the IOCP and kqueue drivers' alone")
)]
#[cfg_attr(
    not(any(windows, target_os = "macos")),
    expect(blaze::unwired, reason = "wired on Windows and macOS; kept elsewhere for its model-checked tests")
)]
pub(crate) struct Wake {
    woken: AtomicBool,
}

#[cfg_attr(
    not(any(windows, target_os = "macos")),
    allow(dead_code, reason = "the IOCP and kqueue drivers' alone")
)]
#[cfg_attr(
    not(any(windows, target_os = "macos")),
    expect(blaze::unwired, reason = "wired on Windows and macOS; kept elsewhere for its model-checked tests")
)]
impl Wake {
    pub(crate) const fn new() -> Wake {
        Wake { woken: AtomicBool::new(false) }
    }

    /// After a hand-back: whether the caller must wake the driver.
    pub(crate) fn wake(&self) -> bool {
        !self.woken.swap(true, Ordering::AcqRel)
    }

    /// Before the driver reads what was handed back.
    pub(crate) fn drain(&self) {
        self.woken.swap(false, Ordering::AcqRel);
    }
}

impl Default for Wake {
    fn default() -> Wake {
        Wake::new()
    }
}

/// Whether a sender's hand-back and the driver's drain, run as two threads by `schedule` in
/// the weak memory ([`threads::two_weak`]: a read may see any value its ordering allows),
/// miss nothing: the sender hands back (a relaxed store of its slot, which only [`Wake`]
/// orders) and calls [`Wake::wake`]; the driver calls [`Wake::drain`] and reads the slot; and
/// where both returned, the driver saw the slot or the sender was told to wake it. `flagged`:
/// the flag as the run begins (a wake-up already on its way to the driver).
///
/// A schedule that ends before a thread does says nothing.
#[cfg(not(blaze_check))]
fn hands(schedule: &[Step], flagged: bool) -> bool {
    let wake = Wake { woken: AtomicBool::new(flagged) };
    let slot = AtomicU32::new(0);
    let (sent, drained) = threads::two_weak(
        schedule,
        || {
            slot.store(1, Ordering::Relaxed);
            wake.wake()
        },
        || {
            wake.drain();
            slot.load(Ordering::Relaxed)
        },
    );
    match (sent, drained) {
        (Ended::Done(must), Ended::Done(seen)) => seen == 1 || must,
        (Ended::Done(_) | Ended::Running | Ended::Asleep, Ended::Done(_) | Ended::Running | Ended::Asleep) => true,
    }
}

/// The invariant (proved: crates/blaze-core/lean/BlazeCore/Handoff.lean): no hand-back is
/// missed, in the weak memory. For every schedule of a sender
/// and the driver, every value each read may see under its ordering, and the flag set or
/// clear as they begin: where both returned, the driver read the sender's slot after its
/// drain, or the sender was told to wake the driver ([`hands`]). It is the orderings that
/// make it so: the two swaps of the flag are one after the other, and the later acquires what
/// the earlier released. The proof is of the weak world's every pick: eleven reachable
/// combinations of the two threads' points, the flag's messages and whose view names the
/// slot's store. Run on drawn schedules below too. Its control is
/// `the_oracle_catches_a_flag_swapped_relaxed`, and in Lean the same two threads with
/// `Relaxed` swaps, a run by evaluation that misses a hand-back (`BlazeCore.HandoffModel`).
#[cfg(not(blaze_check))]
#[cfg_attr(not(test), expect(dead_code, reason = "a theorem function: `blaze prove` reads it"))]
#[blaze::theorem]
pub(crate) fn no_hand_back_is_missed(schedule: &[Step], flagged: bool) -> bool {
    hands(schedule, flagged)
}

#[cfg(test)]
mod tests;
