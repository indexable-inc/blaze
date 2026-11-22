//! Which thread waits for what, and since when: the answer to "why is it hung", readable by the
//! monitor while the program is stuck and without stopping it.
//!
//! One fixed table in the data region the program shares with the monitor, one row per thread
//! (claimed by [`Waits::claim`], given back when the [`WaitSlot`] drops at the thread's end).
//! A row is written only on blocking paths, which cost a sleep anyway: [`WaitSlot::enter`]
//! before the wait, the [`Waiting`] guard's drop after it, on return and on unwind alike.
//! [`Waits::snapshot`] lists the rows that have waited longer than a bound.
//!
//! Each row is a seqlock: one writer (the thread that claimed it), any number of readers, in
//! this process or another, none of which writes. The orderings:
//!
//! - **Writer**: `seq.store(s + 1, Relaxed)` (odd: being written), `fence(Release)`, the fields
//!   with relaxed stores, `seq.store(s + 2, Release)` (even: whole).
//! - **Reader**: `s1 = seq.load(Acquire)`; odd means a write is under way, so try again;
//!   the fields with relaxed loads; `fence(Acquire)`; `s2 = seq.load(Relaxed)`; the row is
//!   whole only when `s1 == s2`.
//!
//! Why a reader never keeps a torn row: if any field load saw a store of a write that began
//! after the write `s1` came from, then the writer's release fence (before that store) and the
//! reader's acquire fence (after that load) synchronize, so the odd store of that write happens
//! before the reader's second load of `seq`, which therefore reads it or later, and `s2 != s1`.
//! If no field load saw a later write, the acquire load of `s1` synchronized with the release
//! store that ended the write `s1` came from, so every field load saw that write's stores. A
//! row caught mid-write is tried again a bounded number of times ([`TRIES`], rule 18) and, if it
//! never settles, counted as unsettled in the [`Snapshot`], never dropped quietly (rule 3).
//!
//! Ownership of a row is its `claimed` word: a compare-exchange from 0 to 1 with `Acquire`
//! claims it, a `Release` store of 0 gives it back, so the last write of one owner happens
//! before the first write of the next and a row never has two writers.
//!
//! Adapted from makepad/relax (MIT), base/src/waits.rs: a fixed table, one row per thread,
//! written only on blocking paths and read lock-free while the waiters are stuck.

use core::fmt;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

use blaze_launch::Thread;

/// What a thread waits for. The integer beside it in a row says which one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum What {
    /// A lazy stub, for a function's code from the control thread. The argument: the slot.
    Code = 1,
    /// The program's main thread, for the entry and the initializers to run. No argument.
    Start = 2,
    /// The control thread, for a thread's report, a stub's need or a frame of the daemon's.
    /// No argument.
    Event = 3,
    /// A read of the daemon's socket. The argument: the file descriptor.
    Read = 4,
    /// A write to the daemon's socket. The argument: the file descriptor.
    Write = 5,
    /// A thread's end, for its rounds of thread-local destructors. The argument: the round.
    End = 6,
    /// A lock. The argument: its address.
    Lock = 7,
    /// A futex or a condition variable. The argument: its address.
    Futex = 8,
    /// A join. The argument: the joined thread's id.
    Join = 9,
    /// A sleep. The argument: how long, in the caller's ticks.
    Sleep = 10,
}

impl What {
    /// The kind a row holds; 0 is a row that waits for nothing.
    fn from_raw(raw: u32) -> Option<Self> {
        Some(match raw {
            1 => Self::Code,
            2 => Self::Start,
            3 => Self::Event,
            4 => Self::Read,
            5 => Self::Write,
            6 => Self::End,
            7 => Self::Lock,
            8 => Self::Futex,
            9 => Self::Join,
            10 => Self::Sleep,
            _ => return None,
        })
    }
}

/// A thread that waits: one whole row as a reader saw it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row {
    /// Which row of the table.
    pub slot: u32,
    pub thread: Thread,
    pub what: What,
    pub arg: u64,
    /// The caller's monotonic tick when the wait began.
    pub since: u64,
}

/// How many times a reader tries a row caught mid-write before it counts it as unsettled.
pub const TRIES: u32 = 64;

/// What [`Waits::snapshot`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Snapshot {
    /// Rows written to the front of `out`.
    pub listed: usize,
    /// Rows past the bound that did not fit in `out`.
    pub not_listed: usize,
    /// Rows still mid-write after [`TRIES`] tries: a writer preempted inside its few stores.
    pub unsettled: usize,
    /// Whole rows naming a kind of wait this build does not know: the region was written by
    /// something other than this layout's writer.
    pub malformed: usize,
}

/// Why a region cannot hold, or does not hold, a table of waits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegionError {
    /// The base is not aligned to [`ALIGN`].
    Misaligned { base: usize },
    /// The region has room for no row: `bytes` against at least [`Waits::bytes_for`]`(1)`.
    TooSmall { base: usize, bytes: usize },
    /// The region's header does not say it is a table of waits of this layout: written by
    /// another blaze, or not a table at all.
    NotATable { base: usize, magic: u64 },
    /// The header names more rows than the region has room for.
    RowsPastEnd { base: usize, rows: u64, bytes: usize },
}

impl fmt::Display for RegionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Misaligned { base } => {
                write!(f, "waits region at {base:#x} is not {ALIGN}-aligned")
            }
            Self::TooSmall { base, bytes } => {
                write!(f, "waits region at {base:#x} has {bytes} bytes: no room for one row")
            }
            Self::NotATable { base, magic } => write!(
                f,
                "waits region at {base:#x} holds magic {magic:#x}, not {MAGIC:#x}: another layout"
            ),
            Self::RowsPastEnd { base, rows, bytes } => write!(
                f,
                "waits region at {base:#x} says {rows} rows, which do not fit in its {bytes} bytes"
            ),
        }
    }
}

impl core::error::Error for RegionError {}

/// Every row is taken: the table was sized for fewer threads than the program runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TableFull {
    pub thread: Thread,
    pub rows: u32,
}

impl fmt::Display for TableFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (rows, thread) = (self.rows, self.thread.0);
        write!(f, "the waits table's {rows} rows are all taken: no row for thread {thread}")
    }
}

impl core::error::Error for TableFull {}

/// A region's base must be aligned to this: a row is one cache line, so two threads' rows never
/// share one.
pub const ALIGN: usize = 64;

/// The header's first word: "blzwait" and the layout's version, 1.
const MAGIC: u64 = u64::from_le_bytes(*b"blzwait\x01");

/// The region's first cache line.
#[repr(C, align(64))]
struct Header {
    magic: AtomicU64,
    rows: AtomicU64,
}

/// One row, a cache line. Shared memory: every field an atomic, so another process may load
/// while this one stores.
#[repr(C, align(64))]
struct RawRow {
    /// Odd while the owner writes the fields below, even when they are whole.
    seq: AtomicU64,
    /// 1 while a thread owns the row, 0 when it is free.
    claimed: AtomicU32,
    /// A [`What`], or 0 for no wait.
    what: AtomicU32,
    thread: AtomicU64,
    arg: AtomicU64,
    since: AtomicU64,
}

impl RawRow {
    /// One seqlock write (the module's orderings). The caller owns the row.
    fn write(&self, thread: Thread, what: u32, arg: u64, since: u64) {
        let s = self.seq.load(Ordering::Relaxed);
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.thread.store(thread.0, Ordering::Relaxed);
        self.what.store(what, Ordering::Relaxed);
        self.arg.store(arg, Ordering::Relaxed);
        self.since.store(since, Ordering::Relaxed);
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// One seqlock read: the fields when whole, `None` when caught mid-write. `CHECK` is false
    /// only in the planted-bug control, which reads without the sequence check.
    fn read<const CHECK: bool>(&self) -> Option<(u64, u32, u64, u64)> {
        let s1 = self.seq.load(Ordering::Acquire);
        if CHECK && s1 % 2 == 1 {
            return None;
        }
        let fields = (
            self.thread.load(Ordering::Relaxed),
            self.what.load(Ordering::Relaxed),
            self.arg.load(Ordering::Relaxed),
            self.since.load(Ordering::Relaxed),
        );
        fence(Ordering::Acquire);
        let s2 = self.seq.load(Ordering::Relaxed);
        if CHECK && s1 != s2 {
            return None;
        }
        Some(fields)
    }
}

/// The table of waits in a shared region: a view, the memory is the caller's.
#[derive(Clone, Copy)]
pub struct Waits<'r> {
    rows: &'r [RawRow],
}

impl<'r> Waits<'r> {
    /// The bytes a region needs for `rows` rows.
    pub const fn bytes_for(rows: u32) -> usize {
        size_of::<Header>() + rows as usize * size_of::<RawRow>()
    }

    /// Makes a fresh table of every row the region has room for: the program's call, once, at
    /// start, before any thread claims a row or the monitor attaches.
    ///
    /// # Safety
    ///
    /// `base..base + bytes` is memory this process may read and write for `'r`, mapped for the
    /// whole of `'r` (a shared mapping the monitor maps too), and nothing else in this process
    /// uses it as anything but this table for `'r`. No other call of `init` or `attach` on it
    /// runs at the same time as this one.
    pub unsafe fn init(base: NonNull<u8>, bytes: usize) -> Result<Self, RegionError> {
        let rows = Self::rows_in(base, bytes)?;
        let header = base.cast::<Header>();
        let first = Self::first_row(base);
        // SAFETY: the caller gives `bytes` writable bytes at `base`, aligned (checked by
        // `rows_in`), and `rows` rows fit after the header (`rows_in`); a header and rows are
        // atomics, valid at every bit pattern once written, so writing each in place before
        // any reference to it exists makes them valid. Nothing else uses the memory meanwhile.
        unsafe {
            header.write(Header { magic: AtomicU64::new(0), rows: AtomicU64::new(rows as u64) });
            for i in 0..rows {
                first.add(i).write(RawRow {
                    seq: AtomicU64::new(0),
                    claimed: AtomicU32::new(0),
                    what: AtomicU32::new(0),
                    thread: AtomicU64::new(0),
                    arg: AtomicU64::new(0),
                    since: AtomicU64::new(0),
                });
            }
        }
        // SAFETY: just written above, valid for `'r` by the caller's promise.
        let (header, rows) =
            unsafe { (header.as_ref(), core::slice::from_raw_parts(first.as_ptr(), rows)) };
        // Release: a monitor that acquires the magic sees the rows' first values.
        header.magic.store(MAGIC, Ordering::Release);
        Ok(Self { rows })
    }

    /// The table another process made with [`Waits::init`]: the monitor's call. It only reads.
    ///
    /// # Safety
    ///
    /// `base..base + bytes` is mapped readable for the whole of `'r`, and is the region the
    /// program gave to `init` (or memory this check refuses: any bytes that are not a header
    /// with [`MAGIC`] are refused before a row is looked at). Atomics are valid at every bit
    /// pattern, so a region the program writes while this reads holds no invalid value.
    pub unsafe fn attach(base: NonNull<u8>, bytes: usize) -> Result<Self, RegionError> {
        let room = Self::rows_in(base, bytes)?;
        // SAFETY: aligned and in bounds (`rows_in`); readable for `'r` by the caller's promise;
        // an `AtomicU64` pair is valid at any bits.
        let header = unsafe { base.cast::<Header>().as_ref() };
        let magic = header.magic.load(Ordering::Acquire);
        if magic != MAGIC {
            return Err(RegionError::NotATable { base: base.addr().get(), magic });
        }
        let rows = header.rows.load(Ordering::Relaxed);
        if rows > room as u64 {
            return Err(RegionError::RowsPastEnd { base: base.addr().get(), rows, bytes });
        }
        // SAFETY: `rows` rows fit after the header (just checked), aligned, readable for `'r`,
        // atomics valid at any bits.
        let rows = unsafe {
            core::slice::from_raw_parts(Self::first_row(base).as_ptr(), rows as usize)
        };
        Ok(Self { rows })
    }

    /// How many rows fit in the region, checking its alignment and size.
    fn rows_in(base: NonNull<u8>, bytes: usize) -> Result<usize, RegionError> {
        let at = base.addr().get();
        if at % ALIGN != 0 {
            return Err(RegionError::Misaligned { base: at });
        }
        let rows = bytes.saturating_sub(size_of::<Header>()) / size_of::<RawRow>();
        // The row index is a `u32` in a `Row`; more rows than that are not used.
        let rows = rows.min(u32::MAX as usize);
        if rows == 0 {
            return Err(RegionError::TooSmall { base: at, bytes });
        }
        Ok(rows)
    }

    fn first_row(base: NonNull<u8>) -> NonNull<RawRow> {
        // SAFETY: the header is one aligned cache line inside the region (`rows_in` found room
        // for it and at least one row), so the address after it is in bounds or one past.
        unsafe { base.add(size_of::<Header>()).cast::<RawRow>() }
    }

    /// How many rows the table has.
    pub fn rows(&self) -> u32 {
        self.rows.len() as u32
    }

    /// Claims a free row for `thread`: the thread's call, once, at its start (threads.rs's
    /// birth). The row is the thread's until the [`WaitSlot`] drops.
    pub fn claim(&self, thread: Thread) -> Result<WaitSlot<'r>, TableFull> {
        for (i, row) in self.rows.iter().enumerate() {
            // Acquire: the previous owner's writes, released by its give-back, happen before
            // this owner's first write, so the row never has two writers.
            if row.claimed.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                row.write(thread, 0, 0, 0);
                return Ok(WaitSlot { row, slot: i as u32, thread, not_sync: PhantomData });
            }
        }
        Err(TableFull { thread, rows: self.rows() })
    }

    /// Writes into `out` the rows that wait and began longer than `bound` ticks before `now`,
    /// in row order. Takes no lock and stops nothing: a row caught mid-write is tried again up
    /// to [`TRIES`] times.
    pub fn snapshot(&self, now: u64, bound: u64, out: &mut [Row]) -> Snapshot {
        self.snapshot_by::<true>(now, bound, out)
    }

    fn snapshot_by<const CHECK: bool>(&self, now: u64, bound: u64, out: &mut [Row]) -> Snapshot {
        let mut found = Snapshot::default();
        for (i, raw) in self.rows.iter().enumerate() {
            let mut tries = 0;
            let fields = loop {
                if let Some(fields) = raw.read::<CHECK>() {
                    break Some(fields);
                }
                tries += 1;
                if tries == TRIES {
                    break None;
                }
                core::hint::spin_loop();
            };
            let Some((thread, what, arg, since)) = fields else {
                found.unsettled += 1;
                continue;
            };
            if what == 0 {
                continue;
            }
            // A whole row of a kind this build does not know: the magic named this layout, so
            // the region was written by something else. Counted, never skipped quietly.
            let Some(what) = What::from_raw(what) else {
                found.malformed += 1;
                continue;
            };
            if now.saturating_sub(since) <= bound {
                continue;
            }
            let row = Row { slot: i as u32, thread: Thread(thread), what, arg, since };
            match out.get_mut(found.listed) {
                Some(to) => {
                    *to = row;
                    found.listed += 1;
                }
                None => found.not_listed += 1,
            }
        }
        found
    }

    /// The planted-bug control: [`Waits::snapshot`] without the sequence check.
    #[cfg(test)]
    pub(crate) fn snapshot_unchecked(&self, now: u64, bound: u64, out: &mut [Row]) -> Snapshot {
        self.snapshot_by::<false>(now, bound, out)
    }
}

/// A thread's row in the table. Not `Clone`, not `Sync`: only its thread writes the row, and
/// [`WaitSlot::enter`] takes `&mut self`, so one wait at a time is written.
pub struct WaitSlot<'r> {
    row: &'r RawRow,
    slot: u32,
    thread: Thread,
    /// A row has one writer: the slot may move to another thread (`Send`), never be shared.
    not_sync: PhantomData<core::cell::Cell<()>>,
}

impl<'r> WaitSlot<'r> {
    /// Which row of the table.
    pub fn slot(&self) -> u32 {
        self.slot
    }

    /// Says this thread now waits for `what` (and `arg`), since `now`: the call just before a
    /// blocking wait. The row says so until the guard drops.
    pub fn enter(&mut self, what: What, arg: u64, now: u64) -> Waiting<'_, 'r> {
        self.row.write(self.thread, what as u32, arg, now);
        Waiting { slot: self }
    }
}

impl Drop for WaitSlot<'_> {
    fn drop(&mut self) {
        // Release: this owner's last write happens before the next owner's claim.
        self.row.claimed.store(0, Ordering::Release);
    }
}

/// A wait under way; its drop says it ended, on return and on unwind.
#[must_use = "the row says the thread waits only while the guard lives"]
pub struct Waiting<'s, 'r> {
    slot: &'s mut WaitSlot<'r>,
}

impl Drop for Waiting<'_, '_> {
    fn drop(&mut self) {
        self.slot.row.write(self.slot.thread, 0, 0, 0);
    }
}

#[cfg(test)]
mod tests;
