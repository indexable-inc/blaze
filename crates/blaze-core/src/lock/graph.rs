//! The wait-for graph's registry, the driver of [`super::stall`]: every wait in progress on a
//! named lock, a [`super::Signal`] or a kernel wait through [`crate::wait`] is a row here from
//! its slow path's start to its end, and [`snapshot`] reads them with each lock's holder (the
//! thread mark its acquire stamped). The daemon's watchdog calls [`stalls`] when a wait passes
//! its threshold.
//!
//! Only slow paths touch it: an uncontended lock never registers.

use std::{
    cell::Cell,
    panic::Location,
    ptr,
    sync::atomic::{AtomicU64, Ordering},
};

use super::{
    Mode,
    stall::{self, Blocked, Snapshot, Stall, WaitRow},
};

/// One registered wait: its row, and for a lock the holder stamp to read at a snapshot.
struct Entry {
    token: u64,
    row: WaitRow,
    /// The lock's holder stamp (`Meta::owner`), for a wait on a lock.
    owner: Option<Owner>,
}

/// A pointer to a lock's holder stamp. The lock outlives the entry: a waiter borrows the lock
/// for as long as it waits, and drops its [`Registered`] (which takes the entry out under
/// [`WAITS`]) before that borrow ends; [`snapshot`] reads the stamp only under [`WAITS`].
struct Owner(*const AtomicU64);

// SAFETY: the pointee is an atomic, read only while the entry is in `WAITS` (see `Owner`).
#[expect(unsafe_code, reason = "a holder stamp read across threads under the registry's lock")]
unsafe impl Send for Owner {}

impl Owner {
    #[expect(unsafe_code, reason = "see `Owner`")]
    fn load(&self) -> u64 {
        // SAFETY: see `Owner`: the entry holding this is in `WAITS`, whose lock the caller
        // holds, so the lock (and its stamp) is still borrowed by its waiter.
        unsafe { &*self.0 }.load(Ordering::Relaxed)
    }
}

/// Every wait in progress. A lock of the lock module's own, touched only on slow paths and by
/// the watchdog, which records nothing of itself (it is the registry).
#[expect(clippy::disallowed_types, reason = "the lock module's own registry, on slow paths only")]
static WAITS: parking_lot::Mutex<Vec<Entry>> = parking_lot::Mutex::new(Vec::new());

static NEXT: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The thread's mark: what a lock's holder is stamped with. 0 until set or first read.
    static MARK: Cell<u64> = const { Cell::new(0) };
}

static NEXT_MARK: AtomicU64 = AtomicU64::new(1 << 32);

/// The calling thread's mark: the id the profiler gave it ([`set_thread_mark`]), else one
/// numbered on first use. Never 0. Reads a thread-local and ends before any wait.
#[inline]
pub fn thread_mark() -> u64 {
    MARK.with(|it| match it.get() {
        0 => {
            let made = NEXT_MARK.fetch_add(1, Ordering::Relaxed);
            it.set(made);
            made
        }
        mark => mark,
    })
}

/// Names the calling thread `mark` in the graph (the profiler's thread id, so a report's
/// threads are the profile's); a simulation sets it per actor before each step. Not 0.
pub fn set_thread_mark(mark: u64) {
    MARK.with(|it| it.set(mark.max(1)));
}

/// A registered wait, removed when it drops.
pub(crate) struct Registered(u64);

impl Drop for Registered {
    fn drop(&mut self) {
        let mut waits = WAITS.lock();
        if let Some(i) = waits.iter().position(|it| it.token == self.0) {
            waits.swap_remove(i);
        }
    }
}

/// Registers the calling thread's wait on `on` from `since_ns`.
pub(crate) fn register(
    on: Blocked,
    span: u64,
    since_ns: u64,
    site: Option<&'static Location<'static>>,
    owner: Option<&AtomicU64>,
) -> Registered {
    let token = NEXT.fetch_add(1, Ordering::Relaxed);
    let row = WaitRow { thread: thread_mark(), span, since_ns, on, holder: None, site };
    // alloc: the registry grows to the most waits at once, once.
    WAITS.lock().push(Entry { token, row, owner: owner.map(|it| Owner(ptr::from_ref(it))) });
    Registered(token)
}

/// Every wait in progress now, each lock wait with its lock's holder.
pub fn snapshot() -> Snapshot {
    let waits = WAITS.lock();
    Snapshot {
        waits: waits
            .iter()
            .map(|it| {
                let mut row = it.row;
                row.holder = it
                    .owner
                    .as_ref()
                    .map(Owner::load)
                    .filter(|&holder| holder != 0 && holder != row.thread);
                row
            })
            .collect(), // alloc: a watchdog's check
    }
}

/// The stalls among the waits in progress older than `threshold_ns` at `now_ns` (on the
/// probe's clock): [`stall::analyze`] of a [`snapshot`].
pub fn stalls(now_ns: u64, threshold_ns: u64) -> Vec<Stall> {
    stall::analyze(&snapshot(), now_ns, threshold_ns)
}

/// The oldest wait in progress's start, for a watchdog's threshold check without a snapshot.
pub fn oldest_since_ns() -> Option<u64> {
    WAITS.lock().iter().map(|it| it.row.since_ns).min()
}

/// A lock's mode as a blocked-on row says it.
pub(crate) fn lock(name: &'static str, mode: Mode, at: usize) -> Blocked {
    Blocked::Lock { name, mode, at }
}
