//! Heat by sampling (D97 point 3 as decided by D100 point 4): the sample ring's entry, and the
//! profile and startup records. Records are keyed by instance SYMBOLS (names that survive edits)
//! and hold no address and no time (point 6).

use blaze_code::record::Symbol;
use blaze_own::{AVec, Alloc, Vec};

/// One sample, as the sampler writes it into its ring: on macOS the control thread with the
/// program thread suspended (`thread_suspend` + `thread_get_state`); on Linux the
/// `timer_create(CLOCK_THREAD_CPUTIME_ID)` signal's handler. Fixed layout: the ring is shared
/// memory between a writer and the attribution reader.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct SampleEntry {
    pub pc: u64,
    pub lr: u64,
    pub fp: u64,
    /// The return address at `[fp + 8]`, or 0 when `fp` was outside the thread's stack bounds
    /// (checked before the read: D100 point 4).
    pub frame_ret: u64,
    /// The code word at `pc` (a `ret` says the caller is in `lr`).
    pub word: u32,
    /// The runtime's index of the sampled thread.
    pub thread: u32,
}

/// One function's samples.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ProfileRow {
    pub symbol: Symbol,
    pub samples: u32,
}

/// One sampled call edge.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct EdgeRow {
    pub caller: Symbol,
    pub callee: Symbol,
    pub samples: u32,
}

/// The profile record of a run: rows sorted by symbol, edges by (caller, callee); unique.
pub struct Profile<'a> {
    pub rows: AVec<'a, ProfileRow>,
    pub edges: AVec<'a, EdgeRow>,
}

impl<'a> Profile<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Profile<'a> {
        Profile { rows: Vec::new_in(alloc), edges: Vec::new_in(alloc) }
    }
}

/// The startup record: the functions in the order the run first called them before `main`
/// returned control to the program (the order IS the content: not sorted; unique).
pub struct Startup<'a> {
    pub order: AVec<'a, Symbol>,
}

impl<'a> Startup<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Startup<'a> {
        Startup { order: Vec::new_in(alloc) }
    }
}
