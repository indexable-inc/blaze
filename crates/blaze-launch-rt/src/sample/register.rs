//! The program's threads, told to the sampler (D100 point 4; the gap piece F1 left). A thread is
//! sampled only once it registered itself with its port (macOS) or its CPU timer (Linux) and its
//! stack bounds; threads.rs's `run` and `adopt` call [`enter`] for each thread the launcher knows,
//! before it runs a byte of the program, and the thread's end gives its [`Entered`] back.
//!
//! The sampler's table is installed once, by the driver that starts sampling ([`install`]),
//! before any program thread starts; a thread entering with no table installed is not sampled and
//! is counted, as is one the table refused (full, a kernel call failed) and a registration that
//! could not be given back: [`counts`] gives the sampler's summary what to report beside its
//! samples, the first refusal by name, so a profile that missed threads says so (rule 3: nothing
//! quietly ignored). A refusal never stops the program: sampling observes it and is not part of
//! what it computes.
//!
//! No std: the table is one pointer, the counts atomics, the first refusal one once-set word.

use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use super::SampleError;

#[cfg(target_os = "linux")]
use super::linux::{Table as Native, Ticket};
#[cfg(target_os = "macos")]
use super::macos::{Threads as Native, Ticket};

/// A sampler's thread table, whatever its allocator: what a thread enters and leaves.
pub trait Register: Sync {
    /// Registers the calling thread as the runtime's thread `index`.
    fn enter(&self, index: u32) -> Result<Ticket, SampleError>;
    /// Ends the calling thread's registration.
    fn leave(&self, ticket: Ticket) -> Result<(), SampleError>;
}

#[cfg(target_os = "macos")]
impl<A: core::alloc::Allocator + Sync> Register for Native<A> {
    fn enter(&self, index: u32) -> Result<Ticket, SampleError> {
        self.register_current(index)
    }

    fn leave(&self, ticket: Ticket) -> Result<(), SampleError> {
        self.end(ticket);
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl<A: core::alloc::Allocator + Clone + Sync + 'static> Register for Native<A> {
    fn enter(&self, index: u32) -> Result<Ticket, SampleError> {
        self.register_current(index)
    }

    fn leave(&self, ticket: Ticket) -> Result<(), SampleError> {
        self.end(ticket)
    }
}

/// What [`install`] is given: the table, behind a thin pointer so it fits one atomic word.
pub struct Installed(pub &'static dyn Register);

/// The installed table. Set once; read by every thread at its start and end.
static TABLE: AtomicPtr<Installed> = AtomicPtr::new(ptr::null_mut());

/// Threads not sampled: no table installed when they entered, a thread number past `u32`, or
/// the table refused them.
static UNSAMPLED: AtomicU64 = AtomicU64::new(0);

/// Registrations the table could not give back.
static UNLEFT: AtomicU64 = AtomicU64::new(0);

/// The first refusal met, entering or leaving, as one word ([`Refusal::word`]); 0 is none. Set
/// once by a compare-exchange from 0: one atomic, so no other memory is published through it and
/// no interleaving can tear it.
static FIRST: AtomicU64 = AtomicU64::new(0);

/// A refusal as the summary reports it: [`SampleError`]'s names are `&'static str`s, which do not
/// fit a word, so the kind and its numbers are kept.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// A kernel call failed with `code` for the runtime's thread `thread` (low 24 bits).
    Kernel { code: i32, thread: u32 },
    /// The thread table of `capacity` (low 24 bits) was full.
    Full { capacity: u32 },
    /// Linux: the thread registered before the sampler started.
    NotStarted,
    /// Any other refusal of the sampler's.
    Other,
}

const KIND_KERNEL: u64 = 1;
const KIND_FULL: u64 = 2;
const KIND_NOT_STARTED: u64 = 3;
const KIND_OTHER: u64 = 4;
const LOW_24: u32 = (1 << 24) - 1;

impl Refusal {
    fn of(error: SampleError) -> Refusal {
        match error {
            SampleError::Kernel { code, thread, .. } => Refusal::Kernel { code, thread: thread & LOW_24 },
            SampleError::Full { capacity } => Refusal::Full { capacity: capacity & LOW_24 },
            SampleError::NotStarted => Refusal::NotStarted,
            _ => Refusal::Other,
        }
    }

    /// Kind in bits 56..64, a 24-bit number in 32..56, a 32-bit code in 0..32; never 0.
    const fn word(self) -> u64 {
        match self {
            Refusal::Kernel { code, thread } => KIND_KERNEL << 56 | ((thread & LOW_24) as u64) << 32 | code as u32 as u64,
            Refusal::Full { capacity } => KIND_FULL << 56 | ((capacity & LOW_24) as u64) << 32,
            Refusal::NotStarted => KIND_NOT_STARTED << 56,
            Refusal::Other => KIND_OTHER << 56,
        }
    }

    const fn from_word(word: u64) -> Option<Refusal> {
        let number = ((word >> 32) as u32) & LOW_24;
        match word >> 56 {
            KIND_KERNEL => Some(Refusal::Kernel { code: word as u32 as i32, thread: number }),
            KIND_FULL => Some(Refusal::Full { capacity: number }),
            KIND_NOT_STARTED => Some(Refusal::NotStarted),
            KIND_OTHER => Some(Refusal::Other),
            _ => None,
        }
    }
}

fn refused(error: SampleError) {
    // Only the first is kept: a later refusal finds the word set and leaves it.
    let _ = FIRST.compare_exchange(0, Refusal::of(error).word(), Ordering::Relaxed, Ordering::Relaxed);
}

/// Why [`install`] refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AlreadyInstalled;

impl core::fmt::Display for AlreadyInstalled {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("the sampler (D100 point 4): a thread table is already installed in this process")
    }
}

impl core::error::Error for AlreadyInstalled {}

/// Makes `table` the one threads register with. Once per process, before program threads start.
pub fn install(table: &'static Installed) -> Result<(), AlreadyInstalled> {
    let given = ptr::from_ref(table).cast_mut();
    TABLE
        .compare_exchange(ptr::null_mut(), given, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| AlreadyInstalled)
}

fn table() -> Option<&'static dyn Register> {
    let at = TABLE.load(Ordering::Acquire);
    // SAFETY: a non-null pointer was stored by `install` from a `&'static Installed`.
    (!at.is_null()).then(|| unsafe { (*at).0 })
}

/// A thread's registration, given back by [`leave`] at its end.
pub struct Entered {
    ticket: Ticket,
}

/// Registers the calling thread, the launcher's thread `thread` (its `table::Thread` number), with
/// the installed table. `None` when it is not sampled; the counts say so.
pub fn enter(thread: u64) -> Option<Entered> {
    let Some(table) = table() else {
        UNSAMPLED.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    // A thread number past `u32` (an entry's `thread` field) is counted, not sampled.
    let Ok(index) = u32::try_from(thread) else {
        UNSAMPLED.fetch_add(1, Ordering::Relaxed);
        return None;
    };
    match table.enter(index) {
        Ok(ticket) => Some(Entered { ticket }),
        Err(error) => {
            UNSAMPLED.fetch_add(1, Ordering::Relaxed);
            refused(error);
            None
        }
    }
}

/// Registers the main thread for the life of the process: its registration is never given back
/// (the main thread ends with the process).
pub fn enter_main(thread: u64) {
    if let Some(entered) = enter(thread) {
        core::mem::forget(entered);
    }
}

/// The calling thread ends: its registration is given back.
pub fn leave(entered: Entered) {
    // An `Entered` exists only once a table was installed, and it never goes.
    let Some(table) = table() else { return };
    if let Err(error) = table.leave(entered.ticket) {
        UNLEFT.fetch_add(1, Ordering::Relaxed);
        refused(error);
    }
}

/// What the sampler's summary reports beside its samples.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Counts {
    /// Threads never sampled.
    pub unsampled: u64,
    /// Registrations not given back.
    pub unleft: u64,
    /// The first refusal met.
    pub first: Option<Refusal>,
}

pub fn counts() -> Counts {
    Counts {
        unsampled: UNSAMPLED.load(Ordering::Relaxed),
        unleft: UNLEFT.load(Ordering::Relaxed),
        first: Refusal::from_word(FIRST.load(Ordering::Relaxed)),
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests {
    use super::{Refusal, SampleError};

    /// A refusal survives its word, and none is 0. Prints
    /// `test sample::register::tests::a_refusal_round_trips_its_word ... ok`.
    #[test]
    fn a_refusal_round_trips_its_word() {
        for refusal in [
            Refusal::Kernel { code: -5, thread: 3 },
            Refusal::Kernel { code: i32::MIN, thread: (1 << 24) - 1 },
            Refusal::Full { capacity: 64 },
            Refusal::NotStarted,
            Refusal::Other,
        ] {
            assert_ne!(refusal.word(), 0);
            assert_eq!(Refusal::from_word(refusal.word()), Some(refusal));
        }
        assert_eq!(Refusal::of(SampleError::AlreadyStarted), Refusal::Other);
        assert_eq!(Refusal::from_word(0), None);
    }
}
