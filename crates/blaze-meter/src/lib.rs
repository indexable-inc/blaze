//! D169, the one meter: what a stretch of one thread used, not only how long it took
//! (docs/v2/plan/design/end-state.md, D169).
//!
//! Two kinds of counter, one snapshot:
//!
//! - **Counted here**: the code that does the work adds to this thread's cells, one
//!   thread-local add each (two where a kind and its time are kept), never a lock or an atomic:
//!   the global heap (blaze-mem `counted.rs`: allocations by size class, bytes, frees, freed
//!   bytes), each frame tag's bytes and resets (blaze-mem `tag::task_ended`), pages mapped
//!   (blaze-mem `tag::committed`), system calls by [`Call`] with their time (the memory layer's
//!   in blaze-mem `syscalls::timed`; effects in bstd `io/fast.rs` `stamp` and `Io::call`; opens
//!   in blaze-core `descriptors.rs`), the store (blaze-cli `engine_records.rs` and
//!   `engine_store.rs`), bytes hashed and digests made (blaze-ids), and lock waits and parks
//!   (blaze-core `lock.rs` and `wait.rs`).
//! - **Read from the system at the snapshot**: this thread's instructions, cycles, user and
//!   system time and page faults ([`System`]). macOS: `thread_selfcounts(THSC_TIME_CPI)`, a
//!   private call (blaze-sys `macos::thread_counts`, rule 45), gives the first four in one
//!   system call; macOS keeps no fault count per thread, so faults are
//!   [`Why::NotPerThread`] there (the process's are in [`Process`]). Linux:
//!   `getrusage(RUSAGE_THREAD)` gives the times and faults, and a `perf_event_open` group (opened
//!   at a thread's first snapshot, kept for its life) the user-mode instructions and cycles,
//!   one `read`; a refused `perf_event_open` (paranoid level, seccomp, a VM without a PMU) is
//!   [`Why::Refused`] with its errno, kept so it is asked once a thread.
//!
//! [`Meter::now`] copies the cells and reads the system: a `Copy` value of this thread.
//! [`Used`] is the difference of two of one owner ([`Whose`]: the thread's serial, the tally,
//! the tests' switch's epoch); any other pair is a typed [`Unpaired`], never a wrapped
//! number, and [`Used`]'s sums are checked ([`Overflow`]); [`Spread`] folds many into a sum and
//! a most. A reading the platform cannot give is a typed [`Reading::Unavailable`], printed as
//! such, never a zero (rule 3).
//!
//! THE COST, stated (rule 67): an allocation is two thread-local adds (its size class's count
//! and its bytes); a free two (count, bytes); a counted effect two (its kind's count, then its
//! time where `Io::call` waits for it, which reads the clock it already reads once more); a
//! store get one or two; a hash update two. A thread-local add is a load, an add and a store at
//! a fixed offset from the thread pointer on Linux, and on macOS the same after the
//! `#[thread_local]` access thunk (`tlv_get_addr`, a few instructions after its first call on a
//! thread). No snapshot is taken per allocation or per effect: snapshots are taken where time
//! is already taken (a phase's mark, a pool task's two clock reads, a body's two), and one
//! costs a copy of [`Cells`] (about 1.1 KB) and of the thread's [`Locks`] (about 1.7 KB:
//! [`LOCK_ROWS`] rows, D171's lock waits) and one system call on macOS, two on Linux
//! (`getrusage` and the perf group's `read`). Nothing of the meter enters a key or a record
//! (rule 25): it has no serialized form.

#![cfg_attr(not(test), no_std)]
#![feature(thread_local, cfg_target_thread_local, register_tool)]
#![register_tool(blaze)]

use core::cell::Cell;
use core::fmt;

#[cfg(test)]
mod tests;

/// The frame tags a thread counts by (blaze-mem's `Tag`, by its index; blaze-mem asserts its
/// tags fit).
pub const TAGS: usize = 24;

/// Size classes of the global heap's allocations: class `c` holds sizes in `[2^c, 2^(c+1))`
/// (0 and 1 in class 0); the last holds every larger one. The classes' sum is the allocation
/// count, and the highest class the stretch touched bounds its largest allocation: a
/// difference of two snapshots keeps both exact, where a running maximum would not.
pub const SIZES: usize = 40;

/// A kind of system call, as the meter counts it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Call {
    /// A file opened for positional reads (blaze-core `descriptors.rs`).
    Open,
    /// `Effect::Stat`, `Lstat`, `Stamp`.
    Stat,
    /// The bulk attribute call: `Effect::Entries` and `List` (a directory's names and
    /// attributes in one call, `getattrlistbulk` on macOS, `getdents64` and `statx` on Linux).
    Attributes,
    /// `Effect::ReadAll`: a whole file.
    Read,
    /// `Effect::Read`, `ReadInto`, `ReadMany`, `ReadLink`: bytes at an offset.
    Pread,
    /// `Effect::Write`, `WriteUnsynced`, `WriteFrom`, `WriteAt`, `Append`, `Truncate`.
    Write,
    /// The memory layer's reserve and fixed maps, and `Effect::Map` and `MapWrite`.
    Mmap,
    Munmap,
    Mprotect,
    /// `Effect::Sync`, `Barrier`, `WriteBack`, `SyncDir`, `Flush`.
    Fsync,
    /// `Effect::Rename`, `RenameUnsynced`.
    Rename,
    /// The file-event calls: `Effect::Watch`, `Journal`, `Unwatch`.
    FileEvents,
    /// Every other effect (spawns, sockets, timers, the clock, the rest of the memory layer).
    Other,
}

/// How many kinds of [`Call`].
pub const CALLS: usize = 13;

impl Call {
    /// Every kind, in the printed order.
    pub const ALL: [Call; CALLS] = [
        Call::Open,
        Call::Stat,
        Call::Attributes,
        Call::Read,
        Call::Pread,
        Call::Write,
        Call::Mmap,
        Call::Munmap,
        Call::Mprotect,
        Call::Fsync,
        Call::Rename,
        Call::FileEvents,
        Call::Other,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Call::Open => "open",
            Call::Stat => "stat",
            Call::Attributes => "bulk attributes",
            Call::Read => "read",
            Call::Pread => "pread",
            Call::Write => "write",
            Call::Mmap => "mmap",
            Call::Munmap => "munmap",
            Call::Mprotect => "mprotect",
            Call::Fsync => "fsync",
            Call::Rename => "rename",
            Call::FileEvents => "file events",
            Call::Other => "other calls",
        }
    }
}

/// A counter one number wide, added to by the code that does the work.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Count {
    /// Bytes the global heap handed out (blaze-mem `counted.rs`).
    HeapBytes,
    /// The global heap's frees, and their bytes.
    HeapFrees,
    HeapFreedBytes,
    /// Bytes of pages the memory layer committed (blaze-mem `tag::committed`).
    Mapped,
    /// Store gets (every ask, a repeat included), distinct keys (each record opened once),
    /// the bytes of the records opened, the bytes copied into the heap, puts and their bytes.
    StoreGets,
    StoreKeys,
    StoreRead,
    StoreCopied,
    StorePuts,
    StorePutBytes,
    /// Bytes fed to BLAKE3 and digests made (blaze-ids).
    Hashed,
    Digests,
    /// Lock waits (a lock taken only after waiting) and their nanoseconds.
    LockWaits,
    LockWaitNs,
    /// Parks of a thread and their nanoseconds.
    Parks,
    ParkNs,
}

/// How many [`Count`]s.
pub const COUNTS: usize = 16;

/// One thread's cells, as [`Meter::now`] copies them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cells {
    pub counts: [u64; COUNTS],
    /// The global heap's allocations by size class ([`SIZES`]).
    pub sizes: [u64; SIZES],
    /// Each frame tag's bytes (its tasks' peaks) and resets.
    pub frame_bytes: [u64; TAGS],
    pub frame_resets: [u64; TAGS],
    /// Each [`Call`]'s count and nanoseconds.
    pub calls: [u64; CALLS],
    pub call_ns: [u64; CALLS],
}

impl Cells {
    pub const ZERO: Cells = Cells {
        counts: [0; COUNTS],
        sizes: [0; SIZES],
        frame_bytes: [0; TAGS],
        frame_resets: [0; TAGS],
        calls: [0; CALLS],
        call_ns: [0; CALLS],
    };

    /// `self + other`, cell by cell (wrapping, as the adds are): a thread's cells and the
    /// worker's [`Tally`] seen as one ([`Meter::now_with`]).
    pub fn plus(&self, other: &Cells) -> Cells {
        let mut out = *self;
        let add = |into: &mut [u64], from: &[u64]| {
            for (into, from) in into.iter_mut().zip(from) {
                *into = into.wrapping_add(*from);
            }
        };
        add(&mut out.counts, &other.counts);
        add(&mut out.sizes, &other.sizes);
        add(&mut out.frame_bytes, &other.frame_bytes);
        add(&mut out.frame_resets, &other.frame_resets);
        add(&mut out.calls, &other.calls);
        add(&mut out.call_ns, &other.call_ns);
        out
    }

    /// The six groups of cells by name, read-only.
    fn groups(&self) -> [(Group, &[u64]); 6] {
        [
            (Group::Counts, &self.counts[..]),
            (Group::Sizes, &self.sizes[..]),
            (Group::FrameBytes, &self.frame_bytes[..]),
            (Group::FrameResets, &self.frame_resets[..]),
            (Group::Calls, &self.calls[..]),
            (Group::CallNs, &self.call_ns[..]),
        ]
    }

    /// The six groups of cells by name, to write.
    fn groups_mut(&mut self) -> [(Group, &mut [u64]); 6] {
        [
            (Group::Counts, &mut self.counts[..]),
            (Group::Sizes, &mut self.sizes[..]),
            (Group::FrameBytes, &mut self.frame_bytes[..]),
            (Group::FrameResets, &mut self.frame_resets[..]),
            (Group::Calls, &mut self.calls[..]),
            (Group::CallNs, &mut self.call_ns[..]),
        ]
    }

    /// `self - before`, cell by cell. Every cell is a counter that only grows: a thread's
    /// cells are only added to ([`bump`]), a [`Tally`] never resets, and a heap free is its own
    /// pair of counters ([`Count::HeapFrees`], [`Count::HeapFreedBytes`]), never a decrement of
    /// an allocation's. So between two snapshots of one owner ([`Whose`]) no cell goes down, and
    /// a cell that did is a pair that is not one owner's: refused naming the cell, never a
    /// wrapped number (rule 76). [`Meter::since`] checks the owners first; this is the same
    /// promise read off the numbers, for a pair the owners could not tell apart (a [`Tally`]
    /// copied and started again at the same address).
    pub fn since(&self, before: &Cells) -> Result<Cells, Unpaired> {
        let mut out = Cells::ZERO;
        for (((group, now), (_, then)), (_, into)) in self.groups().into_iter().zip(before.groups()).zip(out.groups_mut()) {
            for (index, ((now, then), into)) in now.iter().zip(then).zip(into.iter_mut()).enumerate() {
                *into = match now.checked_sub(*then) {
                    Some(grew) => grew,
                    None => return Err(Unpaired::Backwards { field: Field { group, index }, before: *then, now: *now }),
                };
            }
        }
        Ok(out)
    }
}

/// A group of [`Cells`], by its field's name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    Counts,
    Sizes,
    FrameBytes,
    FrameResets,
    Calls,
    CallNs,
}

/// One cell of [`Cells`]: its group and its index there (a [`Count`], a size class, a frame
/// tag's index, a [`Call`]). Printed by its name (print.rs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Field {
    pub group: Group,
    pub index: usize,
}

/// Whose counters a [`Meter`] read: the pair [`Meter::since`] subtracts must be one owner's.
///
/// - `thread`: the thread's serial, given at its first snapshot from a process-wide counter
///   and never given again. Not the address of its cells: a thread that exits frees its
///   thread-local block, and the next thread made is given the same address, so two threads'
///   snapshots looked like one's and a later thread's low counts were subtracted from an
///   earlier one's high ones.
/// - `tally`: the address of the [`Tally`] a [`Meter::now_with`] added, `0` for a snapshot of
///   the thread alone; a thread-only snapshot and a tally one are different sums and never a
///   pair.
/// - `epoch`: how many times the tests' switch ([`testing::stop`]) was thrown before it, `0`
///   in a build without it. A stopped snapshot holds zero cells; a pair across a throw (one
///   real, one stopped) is what made 2026-11-20's `attempt to add with overflow` in
///   [`Used::allocations`]: `0 - (a thread's real count)` wrapped in every size class.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Whose {
    pub thread: u64,
    pub tally: usize,
    pub epoch: u64,
}

/// Why two [`Meter`]s are not a stretch: [`Meter::since`]'s typed refusal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unpaired {
    /// Two threads' snapshots (a task that moved between workers between its two).
    OtherThread,
    /// One with a worker's tally and one without, or two tallies.
    OtherTally,
    /// The tests' switch ([`testing::stop`]) was thrown between them.
    AcrossStop,
    /// A cell lower in the later snapshot than in the earlier: the two are not one owner's
    /// monotone counters ([`Cells::since`]).
    Backwards { field: Field, before: u64, now: u64 },
}

/// A sum of [`Used`]'s cells that passed `u64` ([`Used::allocations`] and its siblings): the
/// sum and the cell whose add overflowed. Not reachable from [`Meter::since`] (each cell is a
/// difference of one thread's counters, their sum one thread's count of events); a sum of
/// many [`Used::plus`] could, and says so instead of wrapping or panicking.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Overflow {
    pub sum: &'static str,
    pub at: Field,
}

/// One worker's own counters (D171): the cells of [`Cells`] as a plain value that its owner
/// adds to through `&mut`, held in the worker's cell (bstd's `Workers`) instead of a
/// thread-local. Code that holds a context adds here (`cx.on_worker(|w| w.meter().call(..))`);
/// code with none (the global allocator, the memory layer's own calls, blaze-core's lock and
/// park waits, blaze-ids' hashing) still adds to the thread's cells, the one named
/// thread-local this crate keeps under C3's allocator exception ([`LOCAL`]'s doc).
///
/// An add is a load, an add and a store at a fixed offset from a pointer the caller already
/// holds: no thread pointer, no access thunk. A tally never resets; a stretch is the
/// difference of two [`Meter`]s, each taken with [`Meter::now_with`] of the same tally.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tally {
    cells: Cells,
    waited: Waited,
}

/// How many named rows a [`Waited`] keeps: the names that waited longest, the rest in its
/// `other` row. Eight: a task's waits are on a few things (its inputs, its children, the
/// store); a stage's on a few more, and the ninth longest is already in the noise beside the
/// first.
pub const WAIT_ROWS: usize = 8;

/// One thing waited on: the wait's static name and, for a wait on a task or a record, that
/// one's id where the waiter has one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WaitRow {
    pub on: &'static str,
    pub id: Option<u64>,
    /// How many waits, and their nanoseconds parked.
    pub count: u64,
    pub ns: u64,
}

/// Where a stretch's waiting went (D171, waits that account themselves): every bstd wait, on
/// its way out, charges its time to the worker it is on, here, by what it waited on. A fixed
/// table: the [`WAIT_ROWS`] names with the most time, and `other` for the rest. A new name
/// when the table is full takes the row of the least time if it is already longer than that
/// row (whose time moves to `other`), else it goes to `other`; so nothing is ever lost:
/// [`Waited::total`] is every wait's time, exactly, and the named rows are the longest
/// within one eviction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Waited {
    pub rows: [Option<WaitRow>; WAIT_ROWS],
    pub other_count: u64,
    pub other_ns: u64,
}

impl Waited {
    pub const ZERO: Waited = Waited { rows: [None; WAIT_ROWS], other_count: 0, other_ns: 0 };

    /// `count` waits of `ns` in all on `on` (`id`).
    pub fn charge(&mut self, on: &'static str, id: Option<u64>, count: u64, ns: u64) {
        if let Some(row) = self.rows.iter_mut().flatten().find(|row| row.on == on && row.id == id) {
            row.count = row.count.wrapping_add(count);
            row.ns = row.ns.wrapping_add(ns);
            return;
        }
        if let Some(free) = self.rows.iter_mut().find(|row| row.is_none()) {
            *free = Some(WaitRow { on, id, count, ns });
            return;
        }
        // Full: the row of the least time.
        let mut least = 0;
        for at in 1..WAIT_ROWS {
            if self.rows[at].map_or(0, |row| row.ns) < self.rows[least].map_or(0, |row| row.ns) {
                least = at;
            }
        }
        match self.rows[least] {
            Some(row) if ns > row.ns => {
                let at = least;
                self.other_count = self.other_count.wrapping_add(row.count);
                self.other_ns = self.other_ns.wrapping_add(row.ns);
                self.rows[at] = Some(WaitRow { on, id, count, ns });
            }
            _ => {
                self.other_count = self.other_count.wrapping_add(count);
                self.other_ns = self.other_ns.wrapping_add(ns);
            }
        }
    }

    /// Every wait's count and nanoseconds, the named rows and `other` together: exact.
    pub fn total(&self) -> (u64, u64) {
        let mut count = self.other_count;
        let mut ns = self.other_ns;
        for row in self.rows.iter().flatten() {
            count = count.wrapping_add(row.count);
            ns = ns.wrapping_add(row.ns);
        }
        (count, ns)
    }

    /// What was waited from `before` to `self` (of one tally). A row in both is the
    /// difference; a row only here is new (or came back after an eviction: then its old time
    /// went to `other`). `other` is what the total grew by less the rows' growth, so the
    /// total is exact whatever was evicted.
    pub fn since(&self, before: &Waited) -> Waited {
        let mut out = Waited::ZERO;
        let (mut rows_count, mut rows_ns) = (0u64, 0u64);
        for (at, row) in self.rows.iter().enumerate() {
            let Some(row) = row else { continue };
            let then = before.rows.iter().flatten().find(|it| it.on == row.on && it.id == row.id);
            let (count, ns) = match then {
                Some(then) if then.ns <= row.ns && then.count <= row.count => (row.count - then.count, row.ns - then.ns),
                _ => (row.count, row.ns),
            };
            if count > 0 || ns > 0 {
                out.rows[at] = Some(WaitRow { count, ns, ..*row });
            }
            rows_count = rows_count.wrapping_add(count);
            rows_ns = rows_ns.wrapping_add(ns);
        }
        let (now_count, now_ns) = self.total();
        let (then_count, then_ns) = before.total();
        out.other_count = now_count.wrapping_sub(then_count).saturating_sub(rows_count);
        out.other_ns = now_ns.wrapping_sub(then_ns).saturating_sub(rows_ns);
        out
    }

    /// Every row of many tables gathered into one, as a function of the multiset of `rows`
    /// and of `other_count` and `other_ns` alone, never of their order (rule 25: a report
    /// merged from workers in any order encodes equal bytes). Equal keys (`on`, `id`) are
    /// summed; the [`WAIT_ROWS`] largest by `ns` are kept, ties broken by `on` then `id`, in
    /// that rank order; the rest go to `other`. `rows` is only reordered (a second call on it
    /// gives the same table). [`Waited::plus`] is the order-dependent form, for a live tally.
    pub fn gathered(rows: &mut [WaitRow], other_count: u64, other_ns: u64) -> Waited {
        rows.sort_unstable_by(|a, b| a.on.cmp(b.on).then(a.id.cmp(&b.id)));
        let mut out = Waited { rows: [None; WAIT_ROWS], other_count, other_ns };
        let mut at = 0;
        while at < rows.len() {
            let mut row = rows[at];
            at += 1;
            while at < rows.len() && rows[at].on == row.on && rows[at].id == row.id {
                row.count = row.count.wrapping_add(rows[at].count);
                row.ns = row.ns.wrapping_add(rows[at].ns);
                at += 1;
            }
            out.keep(row);
        }
        out
    }

    /// `row` placed by rank among the kept rows (kept in rank order), the row it pushes past
    /// [`WAIT_ROWS`], or itself, into `other`.
    fn keep(&mut self, row: WaitRow) {
        let ranks_above = |a: &WaitRow, b: &WaitRow| a.ns > b.ns || (a.ns == b.ns && (a.on, a.id) < (b.on, b.id));
        let mut moving = row;
        for slot in self.rows.iter_mut() {
            let Some(kept) = slot.as_mut() else {
                *slot = Some(moving);
                return;
            };
            if ranks_above(&moving, kept) {
                moving = core::mem::replace(kept, moving);
            }
        }
        self.other_count = self.other_count.wrapping_add(moving.count);
        self.other_ns = self.other_ns.wrapping_add(moving.ns);
    }

    /// The two together, row by row. Which rows are kept past [`WAIT_ROWS`] keys depends on
    /// the order of `plus` calls; a report merged from many tallies uses [`Waited::gathered`].
    pub fn plus(&self, other: &Waited) -> Waited {
        let mut out = *self;
        for row in other.rows.iter().flatten() {
            out.charge(row.on, row.id, row.count, row.ns);
        }
        out.other_count = out.other_count.wrapping_add(other.other_count);
        out.other_ns = out.other_ns.wrapping_add(other.other_ns);
        out
    }

    /// The named row that waited longest.
    pub fn longest(&self) -> Option<WaitRow> {
        self.rows.iter().flatten().copied().max_by_key(|row| row.ns)
    }
}

impl fmt::Display for Waited {
    /// `waited: 41.8 s on names of crate X, 2.1 s on store, 0.3 s other`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut rows = self.rows;
        rows.sort_unstable_by_key(|row| core::cmp::Reverse(row.map_or(0, |row| row.ns)));
        f.write_str("waited:")?;
        let mut first = true;
        for row in rows.iter().flatten() {
            let sep = if first { " " } else { ", " };
            first = false;
            let secs = row.ns / 1_000_000_000;
            let tenths = row.ns / 100_000_000 % 10;
            match row.id {
                Some(id) => write!(f, "{sep}{secs}.{tenths} s on `{}` #{id}", row.on)?,
                None => write!(f, "{sep}{secs}.{tenths} s on `{}`", row.on)?,
            }
        }
        if self.other_ns > 0 || first {
            let sep = if first { " " } else { ", " };
            write!(f, "{sep}{}.{} s other", self.other_ns / 1_000_000_000, self.other_ns / 100_000_000 % 10)?;
        }
        Ok(())
    }
}

/// How many named locks a [`Locks`] keeps, the rest in its `other` row: sixteen, so the
/// report's ten longest are the true ten longest even after a few evictions, at about 1.5 KB.
pub const LOCK_ROWS: usize = 16;

/// What a lock's waiter waited behind (blaze-core `lock.rs` sorts it; this crate is below it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Behind {
    /// A mutex's holder, or a writer behind a writer.
    Holder,
    /// A reader of a `ReadLock` behind a writer.
    ReadBehindWrite,
    /// A writer of a `ReadLock` behind readers.
    WriteBehindRead,
}

/// The holder a lock's waiter waited behind, as it left itself at its release (blaze-core's
/// `Held`, the part the meter keeps).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LockHolder {
    /// The task its thread ran ([`doing`]): `""` for a thread that runs none.
    pub doing: &'static str,
    /// How long it held the lock, to its release.
    pub hold_ns: u64,
    /// Whether `hold_ns` is the whole hold (it waited for the lock, so its acquire time is
    /// known) or only from when the first current waiter arrived (a lower bound).
    pub whole: bool,
}

/// The holder of one lock that made its waiters wait longest.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HolderRow {
    pub doing: &'static str,
    /// The waits it caused, in nanoseconds.
    pub caused_ns: u64,
    /// Its longest hold that made someone wait, and whether that one is whole.
    pub longest_hold_ns: u64,
    pub whole: bool,
}

/// One lock's contention: every acquire of it that waited.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LockRow {
    /// The lock's name (`Lock::new(name, ..)`).
    pub lock: &'static str,
    /// Acquires that waited, and their nanoseconds.
    pub contended: u64,
    pub ns: u64,
    /// The longest one wait.
    pub longest_ns: u64,
    /// Of the waits, a `ReadLock`'s readers behind a writer and writers behind readers.
    pub reads_behind_write: u64,
    pub writes_behind_read: u64,
    /// Waits whose holder let go without leaving itself (it saw no waiter yet, or the wait was
    /// given up).
    pub unseen_holder: u64,
    /// The holder that made it wait longest, within one replacement: a new holder takes the
    /// row when one wait behind it is longer than all the old one caused.
    pub top: Option<HolderRow>,
}

/// Where a stretch's lock waits went, lock by lock (D171's wait table for locks): charged by
/// blaze-core's lock on each contended acquire's way out, never on an uncontended one. A
/// fixed table of the [`LOCK_ROWS`] locks waited on longest and an `other` row, evicted as
/// [`Waited`] is, so [`Locks::total`] is exact and equals the meter's
/// [`Count::LockWaits`]/[`Count::LockWaitNs`] (both are added by [`lock_waited`] from one
/// number): the report checks it.
///
/// Kept apart from [`Waited`] because a lock wait can sit inside a bstd wait (a world's lock
/// taken inside `Cx::wait`): one table of both would count that time twice.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Locks {
    pub rows: [Option<LockRow>; LOCK_ROWS],
    pub other_count: u64,
    pub other_ns: u64,
}

impl Locks {
    pub const ZERO: Locks = Locks { rows: [None; LOCK_ROWS], other_count: 0, other_ns: 0 };

    /// One contended acquire of `lock` that waited `ns` behind `holder`.
    pub fn charge(&mut self, lock: &'static str, behind: Behind, ns: u64, holder: Option<LockHolder>) {
        let mut row = LockRow {
            lock,
            contended: 1,
            ns,
            longest_ns: ns,
            reads_behind_write: u64::from(behind == Behind::ReadBehindWrite),
            writes_behind_read: u64::from(behind == Behind::WriteBehindRead),
            unseen_holder: u64::from(holder.is_none()),
            top: None,
        };
        row.top = holder.map(|it| HolderRow { doing: it.doing, caused_ns: ns, longest_hold_ns: it.hold_ns, whole: it.whole });
        self.merge(row);
    }

    /// `row` added to the table: to its lock's row, a free row, or by eviction.
    pub fn merge(&mut self, row: LockRow) {
        if let Some(have) = self.rows.iter_mut().flatten().find(|it| it.lock == row.lock) {
            have.contended = have.contended.wrapping_add(row.contended);
            have.ns = have.ns.wrapping_add(row.ns);
            have.longest_ns = have.longest_ns.max(row.longest_ns);
            have.reads_behind_write = have.reads_behind_write.wrapping_add(row.reads_behind_write);
            have.writes_behind_read = have.writes_behind_read.wrapping_add(row.writes_behind_read);
            have.unseen_holder = have.unseen_holder.wrapping_add(row.unseen_holder);
            have.top = top_of(have.top, row.top);
            return;
        }
        if let Some(free) = self.rows.iter_mut().find(|it| it.is_none()) {
            *free = Some(row);
            return;
        }
        let mut least = 0;
        for at in 1..LOCK_ROWS {
            if self.rows[at].map_or(0, |it| it.ns) < self.rows[least].map_or(0, |it| it.ns) {
                least = at;
            }
        }
        match self.rows[least] {
            Some(old) if row.ns > old.ns => {
                self.other_count = self.other_count.wrapping_add(old.contended);
                self.other_ns = self.other_ns.wrapping_add(old.ns);
                self.rows[least] = Some(row);
            }
            _ => {
                self.other_count = self.other_count.wrapping_add(row.contended);
                self.other_ns = self.other_ns.wrapping_add(row.ns);
            }
        }
    }

    /// Every contended acquire and its nanoseconds, the rows and `other` together: exact.
    pub fn total(&self) -> (u64, u64) {
        let mut count = self.other_count;
        let mut ns = self.other_ns;
        for row in self.rows.iter().flatten() {
            count = count.wrapping_add(row.contended);
            ns = ns.wrapping_add(row.ns);
        }
        (count, ns)
    }

    /// The stretch from `before` to `self` (of one thread). A longest is a running maximum, so
    /// a stretch's is bounded by its own total: `min(longest, the stretch's ns)`; a top holder
    /// that caused more in the stretch keeps its row, its longest hold the thread's running
    /// one (an upper bound of the stretch's, as no total bounds a hold).
    pub fn since(&self, before: &Locks) -> Locks {
        let mut out = Locks::ZERO;
        let (mut rows_count, mut rows_ns) = (0u64, 0u64);
        for (at, row) in self.rows.iter().enumerate() {
            let Some(row) = row else { continue };
            let then = before.rows.iter().flatten().find(|it| it.lock == row.lock);
            let mut got = *row;
            if let Some(then) = then
                && then.contended <= row.contended
                && then.ns <= row.ns
            {
                got.contended = row.contended - then.contended;
                got.ns = row.ns - then.ns;
                got.reads_behind_write = row.reads_behind_write.wrapping_sub(then.reads_behind_write);
                got.writes_behind_read = row.writes_behind_read.wrapping_sub(then.writes_behind_read);
                got.unseen_holder = row.unseen_holder.wrapping_sub(then.unseen_holder);
                got.longest_ns = row.longest_ns.min(got.ns);
                got.top = match (row.top, then.top) {
                    (Some(now), Some(old)) if now.doing == old.doing => {
                        let caused = now.caused_ns.saturating_sub(old.caused_ns);
                        (caused > 0).then_some(HolderRow { caused_ns: caused, ..now })
                    }
                    (now, _) => now,
                };
            }
            if got.contended > 0 {
                out.rows[at] = Some(got);
            }
            rows_count = rows_count.wrapping_add(got.contended);
            rows_ns = rows_ns.wrapping_add(got.ns);
        }
        let (now_count, now_ns) = self.total();
        let (then_count, then_ns) = before.total();
        out.other_count = now_count.wrapping_sub(then_count).saturating_sub(rows_count);
        out.other_ns = now_ns.wrapping_sub(then_ns).saturating_sub(rows_ns);
        out
    }

    /// The two together, lock by lock.
    pub fn plus(&self, other: &Locks) -> Locks {
        let mut out = *self;
        for row in other.rows.iter().flatten() {
            out.merge(*row);
        }
        out.other_count = out.other_count.wrapping_add(other.other_count);
        out.other_ns = out.other_ns.wrapping_add(other.other_ns);
        out
    }

    /// The named rows, longest total wait first.
    pub fn by_wait(&self) -> [Option<LockRow>; LOCK_ROWS] {
        let mut rows = self.rows;
        rows.sort_unstable_by_key(|row| core::cmp::Reverse(row.map_or(0, |row| row.ns)));
        rows
    }

    /// How many locks it names.
    pub fn named(&self) -> usize {
        self.rows.iter().flatten().count()
    }
}

/// The holder that caused more, or the two's sum when they are one.
fn top_of(have: Option<HolderRow>, new: Option<HolderRow>) -> Option<HolderRow> {
    match (have, new) {
        (Some(have), Some(new)) if have.doing == new.doing => {
            let (longest_hold_ns, whole) = match new.longest_hold_ns > have.longest_hold_ns {
                true => (new.longest_hold_ns, new.whole),
                false => (have.longest_hold_ns, have.whole),
            };
            Some(HolderRow { doing: have.doing, caused_ns: have.caused_ns.wrapping_add(new.caused_ns), longest_hold_ns, whole })
        }
        (Some(have), Some(new)) => Some(if new.caused_ns > have.caused_ns { new } else { have }),
        (have, None) => have,
        (None, new) => new,
    }
}

impl Tally {
    /// A worker's tally before its first add.
    pub const ZERO: Tally = Tally { cells: Cells::ZERO, waited: Waited::ZERO };

    /// A wait of `ns` on `on` (`id`) ended on this worker.
    #[inline]
    pub fn waited(&mut self, on: &'static str, id: Option<u64>, ns: u64) {
        self.waited.charge(on, id, 1, ns);
    }

    /// Where this worker's waiting went.
    pub fn waits(&self) -> &Waited {
        &self.waited
    }

    /// Adds `by` to `count`.
    #[inline(always)]
    pub fn add(&mut self, count: Count, by: u64) {
        let cell = &mut self.cells.counts[count as usize];
        *cell = cell.wrapping_add(by);
    }

    /// One `call` made (its time, if any, by [`Tally::call_took`]).
    #[inline(always)]
    pub fn call(&mut self, call: Call) {
        let cell = &mut self.cells.calls[call as usize];
        *cell = cell.wrapping_add(1);
    }

    /// `ns` of a `call` already counted.
    #[inline(always)]
    pub fn call_took(&mut self, call: Call, ns: u64) {
        let cell = &mut self.cells.call_ns[call as usize];
        *cell = cell.wrapping_add(ns);
    }

    /// A frame of tag index `tag` was reset after holding at most `peak` bytes.
    #[inline]
    pub fn frame_reset(&mut self, tag: usize, peak: u64) {
        let tag = if tag < TAGS { tag } else { TAGS - 1 };
        self.cells.frame_bytes[tag] = self.cells.frame_bytes[tag].wrapping_add(peak);
        self.cells.frame_resets[tag] = self.cells.frame_resets[tag].wrapping_add(1);
    }

    /// What it has counted so far.
    pub fn cells(&self) -> &Cells {
        &self.cells
    }
}

/// The live cells of one thread: `Cell`s at a fixed offset from the thread pointer, `const`
/// and with no destructor, so the allocator may add to them at a thread's first allocation
/// (nothing is registered, nothing allocated).
///
/// C3's allocator exception (D170, D171), named here: the global allocator (blaze-mem
/// `counted.rs`) has no context to find a worker by, so its counts stay in this one
/// thread-local, capped at its fixed size (about 1.1 KB a thread, no growth). Everything that
/// holds a context counts in its worker's [`Tally`] instead. The non-allocator adds that still
/// land here have no context yet (blaze-core `lock.rs`, `wait.rs`, `descriptors.rs`; blaze-ids;
/// blaze-cli's store) and move to a [`Tally`] as their layer takes one.
///
/// Two more words of the same cells, both read and written only on a slow path: `locks`, the
/// thread's lock-wait table ([`lock_waited`], blaze-core's contended acquire, which has no
/// context and cannot be given one without a parameter on all its uncontended takes), and
/// `doing`, the name of the task the thread runs now ([`set_doing`], one store at each pool
/// task's start and end, by bstd), which a lock's holder leaves for its waiter at a release that
/// someone waited for. A lock wait blocks its thread (a fiber never parks inside one: rule 27),
/// so the thread is the worker the task is on for the wait's whole length, and the thread's
/// table is that worker's: a task's [`Meter::now_with`] reads the two together.
struct Local {
    counts: [Cell<u64>; COUNTS],
    sizes: [Cell<u64>; SIZES],
    frame_bytes: [Cell<u64>; TAGS],
    frame_resets: [Cell<u64>; TAGS],
    calls: [Cell<u64>; CALLS],
    call_ns: [Cell<u64>; CALLS],
    locks: Cell<Locks>,
    doing: Cell<&'static str>,
    /// The thread's serial ([`Whose::thread`]): 0 until its first snapshot.
    serial: Cell<u64>,
}

#[cfg(target_thread_local)]
#[thread_local]
static LOCAL: Local = Local {
    counts: [const { Cell::new(0) }; COUNTS],
    sizes: [const { Cell::new(0) }; SIZES],
    frame_bytes: [const { Cell::new(0) }; TAGS],
    frame_resets: [const { Cell::new(0) }; TAGS],
    calls: [const { Cell::new(0) }; CALLS],
    call_ns: [const { Cell::new(0) }; CALLS],
    locks: Cell::new(Locks::ZERO),
    doing: Cell::new(""),
    serial: Cell::new(0),
};

/// One contended acquire of `lock` ended on this thread after `ns` (on the lock's clock),
/// behind `holder`: the meter's lock-wait count and time, and the lock's row, from the one
/// number, so the two always reconcile. Called by blaze-core's lock on its slow path only.
#[cold]
#[inline(never)]
pub fn lock_waited(lock: &'static str, behind: Behind, ns: u64, holder: Option<LockHolder>) {
    with((), |local| {
        bump(&local.counts[Count::LockWaits as usize], 1);
        bump(&local.counts[Count::LockWaitNs as usize], ns);
        let mut locks = local.locks.get();
        locks.charge(lock, behind, ns, holder);
        local.locks.set(locks);
    });
}

/// The task this thread runs now, as the last [`set_doing`] named it: `""` for none.
#[inline]
pub fn doing() -> &'static str {
    with("", |local| local.doing.get())
}

/// This thread runs the task `name` from now; the name it ran before, to put back at the
/// task's end.
#[inline]
pub fn set_doing(name: &'static str) -> &'static str {
    with("", |local| local.doing.replace(name))
}

/// A copy of this thread's lock-wait table.
pub fn locks() -> Locks {
    with(Locks::ZERO, |local| local.locks.get())
}

/// `f` of this thread's cells; on a target with no thread-locals (a guest's wasm without
/// threads) nothing is counted and `none` is the answer: every snapshot there is zero cells,
/// and its system readings say [`Why::NotOnThisSystem`].
#[inline(always)]
fn with<R>(none: R, f: impl FnOnce(&Local) -> R) -> R {
    #[cfg(target_thread_local)]
    {
        _ = none;
        f(&LOCAL)
    }
    #[cfg(not(target_thread_local))]
    {
        _ = f;
        none
    }
}

#[inline(always)]
fn bump(cell: &Cell<u64>, by: u64) {
    cell.set(cell.get().wrapping_add(by));
}

/// Adds `by` to this thread's `count`.
#[inline(always)]
pub fn add(count: Count, by: u64) {
    with((), |local| bump(&local.counts[count as usize], by));
}

/// The size class of an allocation of `bytes`.
#[inline(always)]
const fn size_class(bytes: usize) -> usize {
    let class = (usize::BITS - 1).saturating_sub(bytes.leading_zeros()) as usize;
    if class < SIZES { class } else { SIZES - 1 }
}

/// The global heap handed out `bytes` on this thread: two adds.
#[inline(always)]
pub fn heap_allocated(bytes: usize) {
    with((), |local| {
        bump(&local.sizes[size_class(bytes)], 1);
        bump(&local.counts[Count::HeapBytes as usize], bytes as u64);
    });
}

/// The global heap took back `bytes` on this thread: two adds.
#[inline(always)]
pub fn heap_freed(bytes: usize) {
    with((), |local| {
        bump(&local.counts[Count::HeapFrees as usize], 1);
        bump(&local.counts[Count::HeapFreedBytes as usize], bytes as u64);
    });
}

/// This thread's heap bytes handed out and taken back since it began: what blaze-mem's
/// allocator folds into its shared counters (`counted.rs`), so the two never differ.
#[inline(always)]
pub fn heap_totals() -> (u64, u64) {
    with((0, 0), |local| (local.counts[Count::HeapBytes as usize].get(), local.counts[Count::HeapFreedBytes as usize].get()))
}

/// A task's frame of tag index `tag` was reset after holding at most `peak` bytes.
#[inline]
pub fn frame_reset(tag: usize, peak: u64) {
    let tag = if tag < TAGS { tag } else { TAGS - 1 };
    with((), |local| {
        bump(&local.frame_bytes[tag], peak);
        bump(&local.frame_resets[tag], 1);
    });
}

/// One `call` made on this thread (its time, if any, by [`call_took`]).
#[inline(always)]
pub fn call(call: Call) {
    with((), |local| bump(&local.calls[call as usize], 1));
}

/// `ns` of a `call` already counted.
#[inline(always)]
pub fn call_took(call: Call, ns: u64) {
    with((), |local| bump(&local.call_ns[call as usize], ns));
}

/// A copy of this thread's cells.
pub fn cells() -> Cells {
    let read = |cells: &[Cell<u64>], into: &mut [u64]| {
        for (cell, into) in cells.iter().zip(into) {
            *into = cell.get();
        }
    };
    with(Cells::ZERO, |local| {
        let mut out = Cells::ZERO;
        read(&local.counts, &mut out.counts);
        read(&local.sizes, &mut out.sizes);
        read(&local.frame_bytes, &mut out.frame_bytes);
        read(&local.frame_resets, &mut out.frame_resets);
        read(&local.calls, &mut out.calls);
        read(&local.call_ns, &mut out.call_ns);
        out
    })
}

/// Why a reading is not there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    /// The system refused the call: its errno's name (`ENOTSUP` where the hardware has no
    /// counters, `EACCES` at a perf paranoid level that forbids them).
    Refused(&'static str),
    /// The system counts this for the process only (macOS's page faults).
    NotPerThread,
    /// No call for it is built on this system.
    NotOnThisSystem,
    /// The counters answered zero instructions and cycles for a thread that ran: macOS
    /// "may leave [them] 0 on certain hardware" (sys/resource_private.h).
    ZeroOnThisHardware,
    /// The tests' switch ([`testing::stop`]) turned readings off.
    Stopped,
}

impl Why {
    pub const fn says(self) -> &'static str {
        match self {
            Why::Refused(errno) => errno,
            Why::NotPerThread => "not kept per thread here",
            Why::NotOnThisSystem => "not built on this system",
            Why::ZeroOnThisHardware => "zero on this hardware",
            Why::Stopped => "meter stopped",
        }
    }
}

/// A number read from the system, or why there is none.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reading {
    Got(u64),
    Unavailable(Why),
}

impl Reading {
    /// `self - before`: unavailable if either is.
    pub const fn since(self, before: Reading) -> Reading {
        match (self, before) {
            (Reading::Got(now), Reading::Got(then)) => Reading::Got(now.saturating_sub(then)),
            (Reading::Unavailable(why), _) | (_, Reading::Unavailable(why)) => Reading::Unavailable(why),
        }
    }

    /// `self + other`: unavailable if either is.
    pub const fn plus(self, other: Reading) -> Reading {
        match (self, other) {
            (Reading::Got(a), Reading::Got(b)) => Reading::Got(a.saturating_add(b)),
            (Reading::Unavailable(why), _) | (_, Reading::Unavailable(why)) => Reading::Unavailable(why),
        }
    }

    /// The larger, by value; unavailable if either is.
    pub const fn most(self, other: Reading) -> Reading {
        match (self, other) {
            (Reading::Got(a), Reading::Got(b)) => Reading::Got(if a > b { a } else { b }),
            (Reading::Unavailable(why), _) | (_, Reading::Unavailable(why)) => Reading::Unavailable(why),
        }
    }

    pub const fn got(self) -> Option<u64> {
        match self {
            Reading::Got(n) => Some(n),
            Reading::Unavailable(_) => None,
        }
    }
}

/// What the system says of a thread or the process: instructions, cycles, user and system
/// nanoseconds, minor and major page faults.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct System {
    pub instructions: Reading,
    pub cycles: Reading,
    pub user_ns: Reading,
    pub system_ns: Reading,
    pub minor_faults: Reading,
    pub major_faults: Reading,
}

impl System {
    pub const fn none(why: Why) -> System {
        let none = Reading::Unavailable(why);
        System { instructions: none, cycles: none, user_ns: none, system_ns: none, minor_faults: none, major_faults: none }
    }

    /// Zero of each reading this one has, the rest unavailable as here: the identity of
    /// [`System::plus`] for readings of this kind.
    pub const fn zero_like(self) -> System {
        const fn zero(of: Reading) -> Reading {
            match of {
                Reading::Got(_) => Reading::Got(0),
                unavailable => unavailable,
            }
        }
        System {
            instructions: zero(self.instructions),
            cycles: zero(self.cycles),
            user_ns: zero(self.user_ns),
            system_ns: zero(self.system_ns),
            minor_faults: zero(self.minor_faults),
            major_faults: zero(self.major_faults),
        }
    }

    fn each(self, other: System, f: fn(Reading, Reading) -> Reading) -> System {
        System {
            instructions: f(self.instructions, other.instructions),
            cycles: f(self.cycles, other.cycles),
            user_ns: f(self.user_ns, other.user_ns),
            system_ns: f(self.system_ns, other.system_ns),
            minor_faults: f(self.minor_faults, other.minor_faults),
            major_faults: f(self.major_faults, other.major_faults),
        }
    }

    pub fn since(self, before: System) -> System {
        self.each(before, Reading::since)
    }

    pub fn plus(self, other: System) -> System {
        self.each(other, Reading::plus)
    }

    pub fn most(self, other: System) -> System {
        self.each(other, Reading::most)
    }

    /// Page faults, minor and major together.
    pub const fn faults(self) -> Reading {
        self.minor_faults.plus(self.major_faults)
    }
}

mod os;

/// The next thread serial ([`Whose::thread`]); 0 is "none yet".
static SERIALS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// The calling thread's serial, given at its first call: never another live or past thread's.
/// Only compared, never printed or kept. 0 on a target with no thread-locals (one thread).
fn thread_serial() -> u64 {
    with(0, |local| match local.serial.get() {
        0 => {
            let serial = SERIALS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            local.serial.set(serial);
            serial
        }
        serial => serial,
    })
}

/// A snapshot of this thread's counters ([module docs](crate)): `Copy`, of one owner
/// ([`Whose`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Meter {
    whose: Whose,
    pub cells: Cells,
    pub system: System,
    /// The worker's waits ([`Meter::now_with`]); none in a snapshot of the thread alone.
    pub waited: Waited,
    /// The thread's lock waits by lock ([`lock_waited`]).
    pub locks: Locks,
}

impl Meter {
    /// This thread's counters now: a copy of its cells and one read of the system.
    pub fn now() -> Meter {
        Meter::read(None)
    }

    /// [`Meter::now`] with the worker's own `tally` added to this thread's cells: what a task
    /// that holds a context reads (`cx.on_worker(|w| w.meter().now())`), so the counts its code
    /// made through its worker and those the allocator made on its thread are one snapshot.
    /// A pool worker's cell is its thread's, so the two never mix workers; two snapshots of
    /// one stretch take the same tally on the same thread ([`Whose`] says which).
    pub fn now_with(tally: &Tally) -> Meter {
        Meter::read(Some(tally))
    }

    /// One snapshot: the tests' switch read once (its state and epoch are one word, so a
    /// snapshot is stopped or not as its epoch says), then the system, then the cells.
    fn read(tally: Option<&Tally>) -> Meter {
        let (stopped, epoch) = testing::state();
        let whose = Whose { thread: thread_serial(), tally: tally.map_or(0, |tally| core::ptr::from_ref(tally).addr()), epoch };
        if stopped {
            return Meter { whose, cells: Cells::ZERO, system: System::none(Why::Stopped), waited: Waited::ZERO, locks: Locks::ZERO };
        }
        // The system first, then the cells: what a first read on a thread may do (macOS finds
        // its calls by `dlsym` at their first use) is before this snapshot's cells, never
        // counted to the stretch that starts here.
        let system = os::thread();
        let (cells, waited) = match tally {
            None => (cells(), Waited::ZERO),
            Some(tally) => (cells().plus(tally.cells()), tally.waited),
        };
        Meter { whose, cells, system, waited, locks: locks() }
    }

    /// Whose counters this snapshot read.
    pub const fn whose(&self) -> Whose {
        self.whose
    }

    /// What its owner used from `before` to `self`, or why the two are not one owner's
    /// stretch ([`Unpaired`]): another thread's, another tally's, across the tests' switch,
    /// or a cell that went down. Never a wrapped number, never zeros for a refused pair.
    pub fn since(&self, before: &Meter) -> Result<Used, Unpaired> {
        if self.whose.thread != before.whose.thread {
            return Err(Unpaired::OtherThread);
        }
        if self.whose.tally != before.whose.tally {
            return Err(Unpaired::OtherTally);
        }
        if self.whose.epoch != before.whose.epoch {
            return Err(Unpaired::AcrossStop);
        }
        Ok(Used {
            cells: self.cells.since(&before.cells)?,
            system: self.system.since(before.system),
            waited: self.waited.since(&before.waited),
            locks: self.locks.since(&before.locks),
            overflowed: None,
        })
    }

    /// What this thread used since `self`: [`Meter::now`]`.since(self)`. A snapshot taken
    /// with a tally is refused here ([`Unpaired::OtherTally`]): take the later one with
    /// [`Meter::now_with`] of the same tally.
    pub fn used(&self) -> Result<Used, Unpaired> {
        Meter::now().since(self)
    }
}

/// What a stretch used: the difference of two [`Meter`]s, or a sum of such
/// ([`Used::plus`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Used {
    pub cells: Cells,
    pub system: System,
    /// Where the stretch's waiting went, by what it waited on (the worker's, D171).
    pub waited: Waited,
    /// Its lock waits by lock: the breakdown of [`Count::LockWaits`] and
    /// [`Count::LockWaitNs`], which [`Used::locks_reconcile`] checks.
    pub locks: Locks,
    /// The first cell a [`Used::plus`] saturated at `u64::MAX` instead of wrapping: `None` for
    /// every difference of two snapshots, and printed first where it is not (print.rs).
    pub overflowed: Option<Field>,
}

impl Used {
    /// Whether the lock table's total is the meter's lock-wait count and time (the
    /// UNACCOUNTED rule): both are added from one number by [`lock_waited`], so a difference is
    /// a lock wait that bypassed it.
    pub fn locks_reconcile(&self) -> bool {
        self.locks.total() == (self.count(Count::LockWaits), self.count(Count::LockWaitNs))
    }

    /// Nothing used, every system reading 0: the identity of [`Used::plus`] on a system that
    /// gives every reading. Where one is unavailable, a sum started here says 0 for it; start
    /// a sum from [`Used::zero_like`] of a first `Used` instead, which keeps the
    /// unavailability.
    pub const ZERO: Used = Used {
        cells: Cells::ZERO,
        system: System {
            instructions: Reading::Got(0),
            cycles: Reading::Got(0),
            user_ns: Reading::Got(0),
            system_ns: Reading::Got(0),
            minor_faults: Reading::Got(0),
            major_faults: Reading::Got(0),
        },
        waited: Waited::ZERO,
        locks: Locks::ZERO,
        overflowed: None,
    };

    /// Zero counts, and `self`'s system readings zeroed where it has them.
    pub const fn zero_like(&self) -> Used {
        Used { cells: Cells::ZERO, system: self.system.zero_like(), waited: Waited::ZERO, locks: Locks::ZERO, overflowed: None }
    }

    pub const fn count(&self, count: Count) -> u64 {
        self.cells.counts[count as usize]
    }

    /// The bytes the global heap handed out less those it took back: signed, as a stretch on
    /// one thread may free more than it allocated (it frees what another thread allocated, or
    /// what an earlier stretch did). The one figure of the meter that can go below zero; its
    /// two columns ([`Count::HeapBytes`], [`Count::HeapFreedBytes`]) never do.
    pub const fn heap_kept(&self) -> i128 {
        self.count(Count::HeapBytes) as i128 - self.count(Count::HeapFreedBytes) as i128
    }

    /// The checked sum of `cells` of `group`, or which add passed `u64`.
    fn sum(sum: &'static str, group: Group, cells: &[u64]) -> Result<u64, Overflow> {
        let mut total = 0u64;
        for (index, cell) in cells.iter().enumerate() {
            total = total.checked_add(*cell).ok_or(Overflow { sum, at: Field { group, index } })?;
        }
        Ok(total)
    }

    /// The global heap's allocations: the size classes' sum.
    pub fn allocations(&self) -> Result<u64, Overflow> {
        Used::sum("allocations (the size classes' sum)", Group::Sizes, &self.cells.sizes)
    }

    /// An upper bound of the largest allocation: the top of the highest size class touched
    /// (`2^(c+1)` bytes), `None` with no allocation; the last class has no top (`u64::MAX`).
    pub fn largest_under(&self) -> Option<u64> {
        let class = self.cells.sizes.iter().rposition(|n| *n > 0)?;
        Some(if class + 1 >= SIZES { u64::MAX } else { 1u64 << (class + 1) })
    }

    pub fn frame_bytes(&self) -> Result<u64, Overflow> {
        Used::sum("frame bytes (the tags' sum)", Group::FrameBytes, &self.cells.frame_bytes)
    }

    pub fn frame_resets(&self) -> Result<u64, Overflow> {
        Used::sum("frame resets (the tags' sum)", Group::FrameResets, &self.cells.frame_resets)
    }

    pub const fn calls(&self, call: Call) -> (u64, u64) {
        (self.cells.calls[call as usize], self.cells.call_ns[call as usize])
    }

    /// Lock waits and parks together, and their nanoseconds.
    pub fn waits(&self) -> Result<(u64, u64), Overflow> {
        let field = |count: Count| Field { group: Group::Counts, index: count as usize };
        let waits = self.count(Count::LockWaits).checked_add(self.count(Count::Parks)).ok_or(Overflow { sum: "waits (lock waits and parks)", at: field(Count::Parks) })?;
        let ns = self.count(Count::LockWaitNs).checked_add(self.count(Count::ParkNs)).ok_or(Overflow { sum: "wait time (lock waits' and parks')", at: field(Count::ParkNs) })?;
        Ok((waits, ns))
    }

    /// `f` of each cell pair; where `f` has no answer (a sum past `u64`) the cell saturates
    /// at `u64::MAX` and the first such is kept in [`Used::overflowed`], beside either side's.
    fn each(
        &self,
        other: &Used,
        f: fn(u64, u64) -> Option<u64>,
        system: fn(System, System) -> System,
        waited: fn(&Used, &Used) -> Waited,
        locks: fn(&Used, &Used) -> Locks,
    ) -> Used {
        let mut cells = self.cells;
        let mut overflowed = self.overflowed.or(other.overflowed);
        for ((group, into), (_, from)) in cells.groups_mut().into_iter().zip(other.cells.groups()) {
            for (index, (into, from)) in into.iter_mut().zip(from).enumerate() {
                *into = match f(*into, *from) {
                    Some(value) => value,
                    None => {
                        overflowed = overflowed.or(Some(Field { group, index }));
                        u64::MAX
                    }
                };
            }
        }
        Used { cells, system: system(self.system, other.system), waited: waited(self, other), locks: locks(self, other), overflowed }
    }

    /// The two stretches' use together: checked, a cell past `u64` saturated and named in
    /// [`Used::overflowed`], never wrapped.
    pub fn plus(&self, other: &Used) -> Used {
        self.each(other, u64::checked_add, System::plus, |a, b| a.waited.plus(&b.waited), |a, b| a.locks.plus(&b.locks))
    }

    /// Each counter's larger; the waits, and the lock waits, of the one that waited longer in all.
    pub fn most(&self, other: &Used) -> Used {
        self.each(
            other,
            |a, b| Some(a.max(b)),
            System::most,
            |a, b| match a.waited.total().1 >= b.waited.total().1 {
                true => a.waited,
                false => b.waited,
            },
            |a, b| match a.locks.total().1 >= b.locks.total().1 {
                true => a.locks,
                false => b.locks,
            },
        )
    }

    /// Each counter divided by `n` (a mean, over `n` stretches summed): rounded down.
    pub fn divided(&self, n: u64) -> Used {
        let n = n.max(1);
        let mut cells = self.cells;
        for slice in [&mut cells.counts[..], &mut cells.sizes[..], &mut cells.frame_bytes[..], &mut cells.frame_resets[..], &mut cells.calls[..], &mut cells.call_ns[..]] {
            for value in slice {
                *value /= n;
            }
        }
        let div = |r: Reading| match r {
            Reading::Got(v) => Reading::Got(v / n),
            other => other,
        };
        let s = self.system;
        let system = System {
            instructions: div(s.instructions),
            cycles: div(s.cycles),
            user_ns: div(s.user_ns),
            system_ns: div(s.system_ns),
            minor_faults: div(s.minor_faults),
            major_faults: div(s.major_faults),
        };
        let mut waited = self.waited;
        for row in waited.rows.iter_mut().flatten() {
            row.count /= n;
            row.ns /= n;
        }
        waited.other_count /= n;
        waited.other_ns /= n;
        let mut locks = self.locks;
        for row in locks.rows.iter_mut().flatten() {
            row.contended /= n;
            row.ns /= n;
            row.reads_behind_write /= n;
            row.writes_behind_read /= n;
            row.unseen_holder /= n;
            if let Some(top) = &mut row.top {
                top.caused_ns /= n;
            }
        }
        locks.other_count /= n;
        locks.other_ns /= n;
        Used { cells, system, waited, locks, overflowed: self.overflowed }
    }
}

/// Many stretches of one kind (a stage's tasks, a stage's bodies): their sum, each counter's
/// most, and how many.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Spread {
    pub sum: Used,
    pub most: Used,
    pub n: u64,
    /// Stretches whose two snapshots were refused ([`Meter::since`]): how many, and the first
    /// one's refusal. Counted beside `n`, never as a zero stretch in it.
    pub unpaired: u64,
    pub first_unpaired: Option<Unpaired>,
}

impl Spread {
    pub const NONE: Spread = Spread { sum: Used::ZERO, most: Used::ZERO, n: 0, unpaired: 0, first_unpaired: None };

    /// One stretch more, or its pair's refusal.
    pub fn take_since(&mut self, used: &Result<Used, Unpaired>) {
        match used {
            Ok(used) => self.take(used),
            Err(why) => self.refused(*why),
        }
    }

    /// A stretch whose snapshots were refused.
    pub fn refused(&mut self, why: Unpaired) {
        self.unpaired = self.unpaired.saturating_add(1);
        self.first_unpaired = self.first_unpaired.or(Some(why));
    }

    /// One stretch more.
    pub fn take(&mut self, used: &Used) {
        if self.n == 0 {
            self.sum = used.zero_like();
            self.most = used.zero_like();
        }
        self.sum = self.sum.plus(used);
        self.most = self.most.most(used);
        self.n += 1;
    }

    /// Another spread's stretches (a slice's, joined).
    pub fn join(&mut self, other: &Spread) {
        self.unpaired = self.unpaired.saturating_add(other.unpaired);
        self.first_unpaired = self.first_unpaired.or(other.first_unpaired);
        if other.n == 0 {
            return;
        }
        if self.n == 0 {
            (self.sum, self.most, self.n) = (other.sum, other.most, other.n);
            return;
        }
        self.sum = self.sum.plus(&other.sum);
        self.most = self.most.most(&other.most);
        self.n += other.n;
    }

    /// The mean stretch.
    pub fn mean(&self) -> Used {
        self.sum.divided(self.n)
    }
}

/// The whole process's totals, every thread's since it began: what the stretches' sum is
/// reconciled against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Process {
    pub system: System,
    /// The most resident (macOS: footprint at most) in bytes.
    pub most_resident: Reading,
}

impl Process {
    pub fn now() -> Process {
        os::process()
    }
}

/// The tests' switch: with the meter stopped, every [`Meter::now`] is zero cells and
/// [`Why::Stopped`] readings, so a test compares what a command stores with real readings and
/// with none (rule 25: nothing read by the meter reaches a record). The counting adds go on
/// either way; only the readers see the switch. Not in a build without `testing`.
///
/// Every throw starts an epoch ([`Whose::epoch`]): a snapshot taken before it and one after are
/// refused as a pair ([`crate::Unpaired::AcrossStop`]), so another test's stretch that spans
/// the throw is said to be unmetered, not `0 - (its real counts)` wrapped. The switch and its
/// epoch are one word (`epoch << 1 | stopped`), read once a snapshot, so a snapshot is stopped
/// exactly when its epoch says.
pub mod testing {
    #[cfg(any(test, feature = "testing"))]
    static STATE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

    /// Stops (`true`) or starts the meter's readings, for the whole process: a new epoch
    /// either way.
    #[cfg(any(test, feature = "testing"))]
    pub fn stop(stopped: bool) {
        let next = |state: u64| (((state >> 1) + 1) << 1) | u64::from(stopped);
        let mut state = STATE.load(core::sync::atomic::Ordering::SeqCst);
        // Bounded: each failed exchange is another thread's throw landing first, and only the
        // tests throw it (a handful of times a run).
        for _ in 0..1_000_000 {
            match STATE.compare_exchange_weak(state, next(state), core::sync::atomic::Ordering::SeqCst, core::sync::atomic::Ordering::SeqCst) {
                Ok(_) => return,
                Err(now) => state = now,
            }
        }
        panic!("blaze-meter testing::stop: the switch was thrown by other threads a million times while this throw waited");
    }

    /// Whether the readings are stopped, and the epoch: one read.
    #[inline(always)]
    pub(crate) fn state() -> (bool, u64) {
        #[cfg(any(test, feature = "testing"))]
        return {
            let state = STATE.load(core::sync::atomic::Ordering::SeqCst);
            (state & 1 == 1, state >> 1)
        };
        #[cfg(not(any(test, feature = "testing")))]
        (false, 0)
    }
}

mod print;
pub use print::{Bytes, Grouped, Large, LocksReport, Ns, REPORT_LOCKS};
