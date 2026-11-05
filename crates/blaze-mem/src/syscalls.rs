//! Counts of the kernel's virtual-memory calls this process made through [`crate::os`], each
//! with the time spent in it and its slowest call: what a profile or a bench quotes when it asks
//! how often blaze maps, unmaps, protects or advises memory, and how long the kernel kept it
//! there. On Linux every one of these but a populate and a purge takes the process's
//! `mmap_lock` for writing, so a slow one is a convoy: the faults of every other thread wait
//! behind it (2026-11-05, a cold `blaze check` on 64 cores spent 99.7% of its CPU in the fault
//! handler, each `madvise` and `mmap` taking 4 to 5 s).
//!
//! Relaxed atomics, a pair of clock reads per call: nothing next to a system call that changes
//! the address space.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

/// Which call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `mmap` of a fresh reservation (and its alignment trims, counted as [`Kind::Munmap`]).
    Reserve,
    /// `mmap` with `MAP_FIXED` over part of a reservation: a decommit, or a hugetlb commit.
    MapFixed,
    /// `munmap`: a reservation released, or a reservation's alignment trims.
    Munmap,
    /// `mprotect`: a commit of a `PROT_NONE` reservation.
    Mprotect,
    /// `madvise` that gives pages back (`MADV_DONTNEED`, `MADV_FREE`, macOS' reusable).
    Purge,
    /// `madvise` that prefaults (`MADV_POPULATE_WRITE`).
    Populate,
    /// Any other `madvise` (huge-page advice, `MADV_COLLAPSE`, `MADV_DONTFORK`) and `mbind`.
    Advise,
}

/// Every [`Kind`], in the order [`snapshot`] lists them.
pub const KINDS: [Kind; 7] = [
    Kind::Reserve,
    Kind::MapFixed,
    Kind::Munmap,
    Kind::Mprotect,
    Kind::Purge,
    Kind::Populate,
    Kind::Advise,
];

impl Kind {
    /// Its name in a report.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Reserve => "mmap (reserve)",
            Kind::MapFixed => "mmap (MAP_FIXED)",
            Kind::Munmap => "munmap",
            Kind::Mprotect => "mprotect",
            Kind::Purge => "madvise (purge)",
            Kind::Populate => "madvise (populate)",
            Kind::Advise => "madvise (advice) / mbind",
        }
    }
}

struct Counter {
    calls: AtomicU64,
    ns: AtomicU64,
    max_ns: AtomicU64,
}

static COUNTERS: [Counter; KINDS.len()] = [const {
    Counter { calls: AtomicU64::new(0), ns: AtomicU64::new(0), max_ns: AtomicU64::new(0) }
}; KINDS.len()];

/// What one kind of call came to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Calls {
    pub calls: u64,
    pub ns: u64,
    pub max_ns: u64,
}

/// Runs `call`, a system call of `kind`, counting it and its time.
#[inline]
pub(crate) fn timed<R>(kind: Kind, call: impl FnOnce() -> R) -> R {
    let start = Instant::now();
    let got = call();
    let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let counter = &COUNTERS[kind as usize];
    counter.calls.fetch_add(1, Ordering::Relaxed);
    counter.ns.fetch_add(ns, Ordering::Relaxed);
    counter.max_ns.fetch_max(ns, Ordering::Relaxed);
    got
}

/// The counts so far, one per [`KINDS`] entry.
pub fn snapshot() -> [(Kind, Calls); KINDS.len()] {
    KINDS.map(|kind| {
        let counter = &COUNTERS[kind as usize];
        (
            kind,
            Calls {
                calls: counter.calls.load(Ordering::Relaxed),
                ns: counter.ns.load(Ordering::Relaxed),
                max_ns: counter.max_ns.load(Ordering::Relaxed),
            },
        )
    })
}
