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

use std::{
    alloc::Global,
    fs::File,
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use blaze_hash::HashMap;

use crate::{Cache, FileName, fs, lock::Lock};

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
    kept: Lock<Kept>,
    bound: usize,
    opened: AtomicU64,
    closed: AtomicU64,
    /// Every read opens its file and keeps nothing: the control of the test that a thousand
    /// reads open a file once.
    #[cfg(test)]
    bypass: std::sync::atomic::AtomicBool,
}

struct Kept {
    /// One descriptor a name and cache mode: `F_NOCACHE` (macOS) and `O_DIRECT` (Linux) are the
    /// descriptor's, so a bypassing read never shares a cached one.
    files: HashMap<(FileName, Cache), Entry, Global>,
    /// Counts the reads, for which descriptor was read last.
    clock: u64,
}

struct Entry {
    file: Arc<File>,
    used: u64,
}

impl Kept {
    /// The least recently read descriptor, taken out of the table.
    fn oldest(&mut self) -> Option<Arc<File>> {
        let key = self.files.iter().min_by_key(|(_, entry)| entry.used).map(|(key, _)| key.clone())?; // clone: two refcounts, once a descriptor closed
        self.files.remove(&key).map(|entry| entry.file)
    }
}

impl Descriptors {
    /// A table bounded by [`MOST`] and a quarter of the process's descriptor limit.
    pub(crate) fn new() -> Descriptors {
        Descriptors::bounded(MOST.min(limit() / 4).max(1))
    }

    /// A table of at most `bound` descriptors.
    pub(crate) fn bounded(bound: usize) -> Descriptors {
        assert!(bound > 0, "a table of descriptors keeps at least one");
        Descriptors {
            kept: Lock::new(
                "core.descriptors",
                // Empty until the first read: a driver whose reads are elsewhere (io_uring's, on
                // its ring) allocates nothing for it.
                Kept { files: HashMap::new_in(Global), clock: 0 },
            ),
            bound,
            opened: AtomicU64::new(0),
            closed: AtomicU64::new(0),
            #[cfg(test)]
            bypass: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The descriptor of `name` under `root` for reads through `cache`: the kept one, or the
    /// file opened now and kept, closing the least recently read past the bound. Fails naming
    /// the file.
    pub(crate) fn file(&self, root: &Path, name: &FileName, cache: Cache) -> io::Result<Arc<File>> {
        #[cfg(test)]
        if self.bypass.load(Ordering::Relaxed) {
            // alloc: the control's open, kept by nothing.
            return self.open(root, name, cache).map(Arc::new);
        }
        let key = (name.clone(), cache); // clone: a refcount
        {
            let mut kept = self.kept.lock();
            kept.clock += 1;
            let now = kept.clock;
            if let Some(entry) = kept.files.get_mut(&key) {
                entry.used = now;
                return Ok(Arc::clone(&entry.file)); // clone: a refcount, the read's hold
            }
        }
        // Opened outside the lock: an open is the slow part, and other threads' reads of kept
        // files go on meanwhile.
        // alloc: the shared owner, once a file opened.
        let opened = Arc::new(self.open(root, name, cache)?);
        let (file, closing) = {
            let mut kept = self.kept.lock();
            kept.clock += 1;
            let now = kept.clock;
            match kept.files.get_mut(&key) {
                // Another thread opened it meanwhile: its descriptor is the kept one, ours closes.
                Some(entry) => {
                    entry.used = now;
                    (Arc::clone(&entry.file), Some(opened)) // clone: a refcount
                }
                None => {
                    let closing = match kept.files.len() >= self.bound {
                        true => kept.oldest(),
                        false => None,
                    };
                    // alloc: a slot of the table, which grows to its bound once.
                    kept.files.insert(key, Entry { file: Arc::clone(&opened), used: now }); // clone: a refcount
                    (opened, closing)
                }
            }
        };
        if closing.is_some() {
            self.closed.fetch_add(1, Ordering::Relaxed);
        }
        // Closed outside the lock; a thread reading it still holds its own count, and the
        // descriptor closes as that read ends.
        drop(closing);
        Ok(file)
    }

    /// `name` opened for positional reads, counted.
    fn open(&self, root: &Path, name: &FileName, cache: Cache) -> io::Result<File> {
        let opened = blaze_mem::scratch::with_path(|path| {
            path.push(root);
            path.push(name.as_str());
            fs::open_positional(path, false, cache)
        })
        // alloc: an error path.
        .map_err(|error| io::Error::new(error.kind(), format!("{name}: {error}")))?;
        self.opened.fetch_add(1, Ordering::Relaxed);
        Ok(opened)
    }

    /// The descriptors of `name` and of everything under it closed: the driver is about to
    /// change what the name holds, so the next read opens the file there then.
    pub(crate) fn forget(&self, name: &FileName) {
        let mut kept = self.kept.lock();
        if kept.files.is_empty() {
            return;
        }
        let before = kept.files.len();
        kept.files.retain(|(kept_name, _), _| !kept_name.is_under(name));
        let gone = before - kept.files.len();
        drop(kept);
        self.closed.fetch_add(gone as u64, Ordering::Relaxed);
    }

    pub(crate) fn opens(&self) -> Opens {
        Opens {
            opened: self.opened.load(Ordering::Relaxed),
            closed: self.closed.load(Ordering::Relaxed),
        }
    }

    /// How many descriptors are kept open now.
    #[cfg(test)]
    pub(crate) fn kept(&self) -> usize {
        self.kept.lock().files.len()
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
