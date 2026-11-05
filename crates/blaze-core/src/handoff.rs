//! The two lock-free protocols the drivers run between their threads, kept apart from the
//! drivers so each is model-checked (docs/v2/plan/lanes/safety/model.md) on every platform, not
//! only where its driver runs: [`Claims`], the chunks of one job claimed by many threads and
//! completed by the last (the io_uring driver's stamp sweeps), and [`Wake`], one wake-up of a
//! driver for any number of hand-backs since it last drained them (the IOCP and kqueue drivers'
//! threads).
//! Safe code: what they promise is the memory ordering the unsafe code beside them relies on.

use crate::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// `chunks` pieces of one job, each claimed by exactly one thread, and the job completed by
/// exactly one: the thread whose [`Claims::finish`] counts the last chunk. Every finish is a
/// release and an acquire on one counter (a release sequence of read-modify-writes), so the
/// last finisher sees every write any claimer made to its chunk before finishing it. A claim
/// and a finish are one read-modify-write each, on two words.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "the io_uring driver's alone"))]
pub(crate) struct Claims {
    chunks: usize,
    /// The next chunk to claim.
    next: AtomicUsize,
    /// Chunks finished.
    finished: AtomicUsize,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "the io_uring driver's alone"))]
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
pub(crate) struct Wake {
    woken: AtomicBool,
}

#[cfg_attr(
    not(any(windows, target_os = "macos")),
    allow(dead_code, reason = "the IOCP and kqueue drivers' alone")
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

#[cfg(test)]
mod tests;
