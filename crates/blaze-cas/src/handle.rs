//! The store as code off the daemon's loop holds it for the whole process: a loader that no
//! parameter reaches (rust-analyzer calls a proc-macro expander with none) and that keeps what
//! it made once a process (a module's machine code, what was seen of a module's state).
//!
//! Reads go through a [`Reader`] and take no lock. Puts go through one [`Writer`] of its own
//! behind a lock: they are rare (a module compiled, a verdict seen), readable at once and made
//! durable by the daemon's next [`Store::flush`], which drains every writer's segment. There is
//! one handle a process, made by the daemon once its store is open ([`Handle::install`]).

use std::{path::PathBuf, sync::OnceLock};

use blaze_core::FileName;
use parking_lot::Mutex;

use crate::{Corrupt, Entry, Key, Kind, Mismatch, Pinned, Reader, Store, Ticket, Writer, enter};

pub struct Handle {
    reader: Reader,
    writer: Mutex<Writer>,
    /// The driver's root, which the store's file names are under.
    root: PathBuf,
}

static INSTALLED: OnceLock<Handle> = OnceLock::new();

impl Handle {
    /// A handle on `store`, which must be open, whose driver's root is `root`.
    pub fn new(store: &mut Store, root: PathBuf) -> Handle {
        Handle { reader: store.reader(), writer: Mutex::new(store.writer()), root }
    }

    /// Makes `handle` the process's, for [`Handle::installed`]. A process has one store, so a
    /// second is refused, naming the first's root.
    pub fn install(handle: Handle) -> Result<&'static Handle, String> {
        let mut given = Some(handle);
        let installed = INSTALLED.get_or_init(|| given.take().expect("taken only here"));
        match given {
            None => Ok(installed),
            // alloc: the error of a second store in one process
            Some(_) => Err(format!("the process has a store already, under {}", installed.root.display())),
        }
    }

    /// The process's handle, once the daemon has installed one; none in a process with no store
    /// (a test, a fuzz target, a tool run without the daemon).
    pub fn installed() -> Option<&'static Handle> {
        INSTALLED.get()
    }

    pub fn reader(&self) -> &Reader {
        &self.reader
    }

    /// `with` of the value of `key`, verified, read in place while `with` runs.
    pub fn read<R>(&self, key: &Key, with: impl FnOnce(Entry<'_>) -> R) -> Result<Option<R>, Corrupt> {
        let guard = enter();
        Ok(self.reader.get(&guard, key)?.map(with))
    }

    /// The value of `key`, verified, held for as long as the caller keeps it ([`Pinned`]).
    pub fn pinned(&self, key: &Key) -> Result<Option<Pinned>, Corrupt> {
        let guard = enter();
        self.reader.pinned(&guard, key)
    }

    /// Whether it has a value of `key`, unverified: for planning, never for use.
    pub fn has(&self, key: &Key) -> bool {
        let guard = enter();
        self.reader.has(&guard, key)
    }

    /// Keeps `bytes` under `key`, as [`Writer::put`] does.
    pub fn put(&self, key: Key, kind: Kind, bytes: Vec<u8>) -> Result<Ticket, Mismatch> {
        let guard = enter();
        // A put that panicked left its segment whole: a writer publishes only whole records.
        let mut writer = self.writer.lock();
        writer.put(&guard, key, kind, bytes)
    }

    /// Whether the put that got `ticket` is durable.
    pub fn durable(&self, ticket: Ticket) -> bool {
        self.reader.durable(ticket)
    }

    /// Where `file`, one of the store's ([`Pinned::file`]), is on this machine.
    pub fn path_of(&self, file: &FileName) -> PathBuf {
        self.root.join(file.as_str())
    }
}
