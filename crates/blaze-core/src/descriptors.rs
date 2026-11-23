//! The descriptors a driver's file threads keep open for reads into memory
//! ([`crate::Effect::ReadInto`], [`crate::Effect::ReadMany`]): a file is opened once, at its
//! first read, and every later read of it is one `pread` (or one `preadv` a run) on the kept
//! descriptor. An open costs about 15 us against about 0.6 us for a 256-byte read from the page
//! cache, so the open was the read's cost. io_uring keeps its own (`uring`'s `positional`, in the
//! ring's table of registered files); this is the threads', kqueue's and IOCP's.
//!
//! The bound: at most [`MOST`] descriptors, and never more than a quarter of the process's
//! `RLIMIT_NOFILE`, so the table leaves the rest of the limit to sockets, pipes, watches and the
//! files every other effect opens and closes. 1024 holds every pack of the store's direct
//! reader ([`blaze_cas::direct`] keeps at most 64 files) many times over, and a kept descriptor
//! costs the kernel one file object, a few hundred bytes. Past the bound the least recently read
//! descriptor is closed: a read is never refused for room.
//!
//! What a reader sees, the contract the store's packs keep: a kept descriptor reads the file
//! it opened (its inode), not whatever the name points at later. The files read this way are
//! content-named and immutable below their committed length (appended to, never cut or written
//! in place), so a descriptor opened before a rename over the name, or an unlink, reads the same
//! bytes the name held when it was opened, which are the bytes asked for. The driver's own
//! effects that change what is under a name ([`crate::Effect::Remove`], `Rename` and
//! `RenameUnsynced` (both names), `Write`, `WriteUnsynced`, `Truncate`, `Clone` and `Link` (the
//! name made), `RemoveDir`) close the name's descriptor first, so a read after them opens the
//! file now there, and a read after the driver removed a file fails naming it. A file another
//! process (the collector of another daemon) unlinks stays held open until this table closes it
//! past its bound or the driver ends: at most [`MOST`] such inodes, and reading one returns the
//! bytes it held, never another file's.
//!
//! # A read of a kept file takes no lock
//!
//! Before: one `Lock<Kept>` around the whole table, taken by every read (a hit too, to advance a
//! clock of reads and stamp the entry), so every file thread of a driver queued on one mutex per
//! read (the outside review, thoughts.md 2.4). Now ([`Table`]) the table is two halves:
//!
//! - what a read finds, a [`SharedMap`] read with no lock (blaze-hash shared.rs: a probe of a
//!   pinned table, entries reclaimed by epoch), whose entry is the descriptor's [`Weak`] and when
//!   it was last read;
//! - what keeps each descriptor open, its one strong [`Arc`], in a map under the one lock
//!   `core.descriptors`, which only a miss (after its open), an eviction and a forget take.
//!
//! A hit is a probe, one load of the table's tick (stored into the entry only when it moved, so a
//! steady set of hot files writes no shared line but the `Arc`'s count), and one
//! [`Weak::upgrade`]: the read's hold. Its floor is that: no lock, one atomic add on the
//! descriptor's count, which the read's hold needs anyway.
//!
//! Why the found half holds a `Weak`, not the `Arc`: an entry the map removes is retired, and
//! freed only once every pin that could see it has gone (the map frees retired entries in
//! batches of 32 a shard). An `Arc` there would keep an evicted descriptor open that long, up to
//! 2,048 past the bound across the map's 64 shards; with the strong count in the locked half, an
//! evicted descriptor closes as soon as the last read through it ends.
//!
//! Recency is at the grain of one keep: the tick moves once per descriptor kept, and a read
//! stamps its entry with the tick. The descriptor evicted has the oldest stamp: none read since
//! an earlier keep than any other's. Ties go to the map's order, which is the same on every run.
//!
//! The invariant (proved by the interleaving DST in descriptors/tests.rs, with a planted
//! control for each line, [`TablePlant`]): outside an operation,
//!
//! 1. at most `bound` descriptors are kept, and every open one is kept, held by a read, or
//!    opened by a read not yet kept ([`TablePlant::Lingers`], [`TablePlant::PastBound`]);
//! 2. a read's descriptor stays open while it holds it, evicted or forgotten meanwhile (by type:
//!    the hold is an `Arc`);
//! 3. a descriptor whose open began before a forget of its name is never kept after that forget
//!    ([`TablePlant::KeptAfterForget`]): a read that raced the driver's change keeps nothing;
//! 4. a read's descriptor and a write's ([`Access`]) of one file are two entries, each found
//!    only under its own key, and a forget of the name lets both go.
//!
//! What the guard of line 3 does not close: an open that begins after the forget and before the
//! driver's change itself (a read issued while the write runs) may keep the file the change
//! replaces. The driver's effects call `forget` before the change; a forget after it too would
//! close that window (the threads', kqueue's and IOCP's drivers, not this file).

use std::{
    alloc::Global,
    fs::File,
    io,
    path::Path,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use blaze_hash::{FixedState, Map, MapOpts, SharedMapOpts};
use blaze_own::{
    Share,
    planted::{NoPlant, Planted, Plants},
};

use crate::{
    Cache, FileName, fs,
    lock::{Lock, SharedMap},
};

#[cfg(test)]
mod tests;

/// The most descriptors one table keeps open; fewer where `RLIMIT_NOFILE / 4` is lower.
pub(crate) const MOST: usize = 1024;

/// How many descriptors a driver's table opened and closed since it started: a thousand reads of
/// one file open it once.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Opens {
    /// Files opened for reads into memory.
    pub opened: u64,
    /// Descriptors closed: past the bound, or let go because the driver changed their file.
    pub closed: u64,
}

/// The descriptors kept open, shared by a driver's file threads.
pub(crate) struct Descriptors {
    table: Table<File>,
    opened: AtomicU64,
    /// Every read opens its file and keeps nothing: the control of the test that a thousand
    /// reads open a file once.
    #[cfg(test)]
    bypass: std::sync::atomic::AtomicBool,
}

/// One descriptor a name, cache mode and access: `F_NOCACHE` (macOS) and `O_DIRECT` (Linux)
/// are the descriptor's, so a bypassing read never shares a cached one, and a descriptor opened
/// for writing is its own entry, never the one reads share.
pub(crate) type Key = (FileName, Cache, Access);

/// What a kept descriptor was opened for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Access {
    /// Positional reads ([`Descriptors::file`]).
    Read,
    /// Positional reads and writes ([`Descriptors::writable`]): `WriteAt`, `WriteFrom`,
    /// `Truncate`, `Append` and the seal's barrier.
    Write,
}

/// The bugs a control plants in a [`Table`] (descriptors/tests.rs), each the break of one line
/// of the module's invariant. Shipped code passes [`NoPlant`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TablePlant {
    /// The table as it is.
    #[cfg_attr(not(test), allow(dead_code, reason = "the clean run of descriptors/tests.rs's controls"))]
    None,
    /// A keep compares no forget: a descriptor opened before a forget of its name is kept after
    /// it, so a read after the driver's change reads the file the change replaced.
    KeptAfterForget,
    /// A keep at the bound evicts nothing: the table grows past it.
    PastBound,
    /// An evicted descriptor stays open in the table's keeping (as one held by a retired entry of
    /// the found half would, until the map's batch frees it), not only until its last read ends.
    Lingers,
}

impl Plants for TablePlant {}

/// What a read finds of a kept descriptor, with no lock.
struct Found<H> {
    /// The descriptor, while the table or a read keeps it open.
    file: Weak<H>,
    /// The table's tick at the last read of it.
    used: AtomicU64,
}

/// What keeps the table's descriptors open: the half under its lock.
struct Owned<H> {
    files: Map<Key, Arc<H>, FixedState, Global>,
    /// Evicted descriptors kept open by [`TablePlant::Lingers`] alone: empty in shipped code.
    lingering: std::vec::Vec<Arc<H>, Global>,
}

/// A miss's open begun: what the table had forgotten when it began ([`Table::opening`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Opening {
    forgets: u64,
}

/// The bounded table of kept descriptors `H` (a [`File`] where it ships; the DST's own handle in
/// its test), read with no lock: the module's docs say how and what it promises.
pub(crate) struct Table<H> {
    found: SharedMap<Key, Found<H>, Global>,
    owned: Lock<Owned<H>>,
    bound: usize,
    /// Moves once per descriptor kept: what a read stamps its entry with.
    tick: AtomicU64,
    /// Moves once per forget, under the lock, before its removals.
    forgets: AtomicU64,
    /// Descriptors let go: evicted, forgotten, or opened by a read and not kept.
    closed: AtomicU64,
}

impl<H: Send + Sync + 'static> Table<H> {
    /// A table of at most `bound` descriptors, empty: a driver whose reads are elsewhere (io_uring's,
    /// on its ring) allocates no shard's table for it.
    pub(crate) fn new(bound: usize) -> Table<H> {
        assert!(bound > 0, "a table of descriptors keeps at least one");
        Table {
            found: SharedMap::new(SharedMapOpts { name: "core.descriptors.found", hasher: FixedState, alloc: Global, capacity: None }),
            owned: Lock::new(
                "core.descriptors",
                Owned {
                    files: Map::new(MapOpts { hasher: FixedState, alloc: Global, capacity: None }),
                    lingering: std::vec::Vec::new_in(Global),
                },
            ),
            bound,
            tick: AtomicU64::new(0),
            forgets: AtomicU64::new(0),
            closed: AtomicU64::new(0),
        }
    }

    /// The kept descriptor of `key`, held for the read, with no lock; `None` when none is kept
    /// (or it was evicted as it was found): the read opens it ([`Table::opening`],
    /// [`Table::keep`]).
    pub(crate) fn get(&self, key: &Key) -> Option<Arc<H>> {
        let pinned = self.found.pin();
        let found = pinned.get(key)?;
        // Relaxed, both: a stamp for the eviction's choice, never a guard of anything.
        let now = self.tick.load(Ordering::Relaxed);
        if found.used.load(Ordering::Relaxed) != now {
            found.used.store(now, Ordering::Relaxed);
        }
        found.file.upgrade()
    }

    /// A miss's open begins: called before the open, its answer handed to [`Table::keep`].
    pub(crate) fn opening(&self) -> Opening {
        // Acquire, pairing with a forget's Release: a forget that happened before this open
        // began is seen, so its descriptor may be kept; one that did not is caught under the
        // lock by `keep`.
        Opening { forgets: self.forgets.load(Ordering::Acquire) }
    }

    /// `opened` (the read's open of `key`, begun at `opening`) kept, closing the least recently
    /// read past the bound; the read's hold. Another read's descriptor kept meanwhile is the
    /// kept one, and `opened` closes as this call returns; one a forget may have let go while it
    /// opened is the read's alone, kept by nothing.
    pub(crate) fn keep(&self, key: Key, opening: Opening, opened: Arc<H>, plant: impl Planted<TablePlant>) -> Arc<H> {
        let (file, let_go) = {
            let mut owned = self.owned.lock();
            let pinned = self.found.pin();
            if let Some(held) = owned.files.get(&key) {
                let held = Arc::clone(held); // clone: a refcount, the read's hold
                if let Some(found) = pinned.get(&key) {
                    found.used.store(self.tick.load(Ordering::Relaxed), Ordering::Relaxed);
                }
                (held, Some(opened))
            } else if self.forgets.load(Ordering::Relaxed) != opening.forgets && !plant.has(TablePlant::KeptAfterForget) {
                // Relaxed: forgets move under the lock held here. The open may be of the file a
                // forget let go: the read takes it, the table does not keep it.
                self.closed.fetch_add(1, Ordering::Relaxed);
                return opened;
            } else {
                let mut evicted = None;
                if owned.files.len() >= self.bound && !plant.has(TablePlant::PastBound) {
                    // The oldest stamp; under the lock both halves hold the same keys.
                    let oldest = pinned.iter().min_by_key(|(_, found)| found.used.load(Ordering::Relaxed)).map(|((name, cache, access), _)| (name.share(), *cache, *access)); // clone: two refcounts, once a descriptor closed
                    if let Some(oldest) = oldest {
                        pinned.remove(&oldest);
                        evicted = owned.files.remove(&oldest);
                    }
                }
                if plant.has(TablePlant::Lingers)
                    && let Some(evicted) = evicted.take()
                {
                    owned.lingering.push(evicted);
                    self.closed.fetch_add(1, Ordering::Relaxed);
                }
                let now = self.tick.fetch_add(1, Ordering::Relaxed) + 1;
                // alloc: an entry of each half, once a descriptor kept.
                pinned.insert((key.0.share(), key.1, key.2), Found { file: Arc::downgrade(&opened), used: AtomicU64::new(now) }); // clone: a refcount
                owned.files.insert(key, Arc::clone(&opened)); // clone: a refcount, the table's
                (opened, evicted)
            }
        };
        if let_go.is_some() {
            self.closed.fetch_add(1, Ordering::Relaxed);
        }
        // Closed outside the lock; a read through it still holds its own count, and the
        // descriptor closes as that read ends.
        drop(let_go);
        file
    }

    /// The descriptors of `name` and of everything under it let go: the driver is about to
    /// change what the name holds, so the next read opens the file there then, and a read whose
    /// open began before this keeps nothing.
    pub(crate) fn forget(&self, name: &FileName) {
        let mut owned = self.owned.lock();
        // Release: see `opening`.
        self.forgets.fetch_add(1, Ordering::Release);
        if owned.files.is_empty() {
            return;
        }
        let before = owned.files.len();
        // The found half first: a read that finds the entry after this upgrades nothing once the
        // strong count below is let go, and opens anew.
        // Both accesses of each name: a read's descriptor and a write's.
        self.found.pin().retain(|(kept, _, _), _| !kept.is_under(name));
        owned.files.retain(|(kept, _, _), _| !kept.is_under(name));
        let gone = before - owned.files.len();
        drop(owned);
        self.closed.fetch_add(gone as u64, Ordering::Relaxed);
    }

    /// Descriptors let go since the table was made.
    pub(crate) fn closed(&self) -> u64 {
        self.closed.load(Ordering::Relaxed)
    }

    /// How many descriptors are kept open now.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.owned.lock().files.len()
    }

    /// Each kept descriptor with its key, and the count of the found half: the DST's view.
    #[cfg(test)]
    pub(crate) fn each_kept(&self, mut each: impl FnMut(&Key, &Arc<H>)) -> usize {
        let owned = self.owned.lock();
        for (key, file) in owned.files.iter() {
            each(key, file);
        }
        self.found.len()
    }
}

impl Descriptors {
    /// A table bounded by [`MOST`] and a quarter of the process's descriptor limit.
    pub(crate) fn new() -> Descriptors {
        Descriptors::bounded(MOST.min(limit() / 4).max(1))
    }

    /// A table of at most `bound` descriptors.
    pub(crate) fn bounded(bound: usize) -> Descriptors {
        Descriptors {
            table: Table::new(bound),
            opened: AtomicU64::new(0),
            #[cfg(test)]
            bypass: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The descriptor of `name` under `root` for reads through `cache`: the kept one, found with
    /// no lock, or the file opened now and kept, closing the least recently read past the bound.
    /// Fails naming the file.
    pub(crate) fn file(&self, root: &Path, name: &FileName, cache: Cache) -> io::Result<Arc<File>> {
        self.descriptor(root, name, cache, Access::Read)
    }

    /// The descriptor of `name` under `root` opened for positional writes (and reads) through
    /// `cache`, kept as [`Descriptors::file`] keeps a read's: its own entry beside the read's
    /// descriptor of the same file, let go by the same [`Descriptors::forget`] and the same
    /// bound, counted in the same [`Opens`]. Fails naming the file.
    pub(crate) fn writable(&self, root: &Path, name: &FileName, cache: Cache) -> io::Result<Arc<File>> {
        self.descriptor(root, name, cache, Access::Write)
    }

    fn descriptor(&self, root: &Path, name: &FileName, cache: Cache, access: Access) -> io::Result<Arc<File>> {
        #[cfg(test)]
        if self.bypass.load(Ordering::Relaxed) {
            // alloc: the control's open, kept by nothing.
            return self.open(root, name, cache, access).map(Arc::new);
        }
        let key = (name.share(), cache, access); // clone: a refcount
        if let Some(file) = self.table.get(&key) {
            return Ok(file);
        }
        // Opened outside the lock: an open is the slow part, and other threads' reads of kept
        // files go on meanwhile.
        let opening = self.table.opening();
        // alloc: the shared owner, once a file opened.
        let opened = Arc::new(self.open(root, name, cache, access)?);
        Ok(self.table.keep(key, opening, opened, NoPlant))
    }

    /// `name` opened for positional reads, and writes for [`Access::Write`], counted.
    fn open(&self, root: &Path, name: &FileName, cache: Cache, access: Access) -> io::Result<File> {
        let opened = blaze_mem::scratch::with_path(|path| {
            path.push(root);
            path.push(name.as_str());
            fs::open_positional(path, access == Access::Write, cache)
        })
        // alloc: an error path.
        .map_err(|error| io::Error::new(error.kind(), format!("{name}: {error}")))?;
        self.opened.fetch_add(1, Ordering::Relaxed);
        // The meter (D169): an open, on the thread that ran the read that needed it (the
        // asker's for an inline call, an IO thread's for a queued one).
        blaze_meter::call(blaze_meter::Call::Open);
        Ok(opened)
    }

    /// The descriptors of `name` and of everything under it closed: the driver is about to
    /// change what the name holds, so the next read opens the file there then.
    pub(crate) fn forget(&self, name: &FileName) {
        self.table.forget(name);
    }

    pub(crate) fn opens(&self) -> Opens {
        Opens { opened: self.opened.load(Ordering::Relaxed), closed: self.table.closed() }
    }

    /// How many descriptors are kept open now.
    #[cfg(test)]
    pub(crate) fn kept(&self) -> usize {
        self.table.len()
    }

    /// Every read opens its file and keeps nothing from now on: the planted bug of the test that
    /// a thousand reads open a file once.
    #[cfg(test)]
    pub(crate) fn bypass(&self) {
        self.bypass.store(true, Ordering::Relaxed);
    }
}

/// The process's descriptor limit, as a count of descriptors.
#[cfg(unix)]
fn limit() -> usize {
    let limit = crate::raw::nofile()
        .expect("getrlimit(RLIMIT_NOFILE) into a live rlimit fails only for an unknown resource");
    usize::try_from(limit).unwrap_or(usize::MAX)
}

/// Windows has no small per-process limit of handles (about 16 million), and WASI none blaze
/// reads: the table's own bound is the bound.
#[cfg(not(unix))]
fn limit() -> usize {
    usize::MAX
}
