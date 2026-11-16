//! The simulated disk's ledger (docs/v2/plan/design/std.md, "Files: whether bytes reached the
//! disk is in the type"): what blaze-core's simulation does not model and a test of file code
//! must be able to assert.
//!
//! - **What was paid.** [`DiskCounts`]: bytes written and read, flushes asked, the flushes and
//!   name syncs a caller waited for, clones, and the bytes a clone forced out because its
//!   source was not flushed (`forced_writeback`). A test of an edit path asserts the last is
//!   zero, with no real disk.
//!
//! Which names a crash may undo is blaze-core's simulation's, not the ledger's: a name is
//! durable only once its directory is synced (`Effect::SyncDir`; `Effect::Write` and
//! `Effect::Rename` sync theirs before they complete), and a crash undoes what no directory
//! sync covered (`blaze_core::sim`, "The disk").
//!
//! The ledger exists only in a simulated world ([`crate::Io::disk`] is `None` on a real
//! driver). It reads every effect as it is handed to the driver and every completion as it is
//! polled, so no file call can forget to tell it.

use alloc::vec::Vec;
use blaze_core::{Completion, Done, Effect, FileName, Tag};

/// What the simulated disk was asked to do, counted since the world was made
/// ([`crate::Io::disk`]). Every field only grows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DiskCounts {
    /// Bytes handed to the disk by appends, positional writes and whole-file writes.
    pub written: u64,
    /// Bytes file reads brought back.
    pub read: u64,
    /// Flushes of a file's bytes asked of the disk, waited for or not.
    pub flushes: u64,
    /// Write-backs begun (`Durable::Started`): bytes sent toward the device with nothing
    /// waiting and nothing made durable.
    pub writebacks: u64,
    /// Syncs of a directory's names.
    pub name_syncs: u64,
    /// Flushes and name syncs a caller waited for: the waits for the device. A path built
    /// only of `_unsynced` calls leaves it as it was.
    pub device_waits: u64,
    /// Clones made.
    pub clones: u64,
    /// Bytes clones forced out: what their sources had been written and not flushed when
    /// they were cloned. A clone of a flushed file adds nothing.
    pub forced_writeback: u64,
}

/// The directory `name` is in: everything before its last `/`, or the root's empty name.
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn dir_of(name: &FileName) -> &str {
    match name.as_str().rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// The ledger of one simulated world.
pub(crate) struct Disk {
    counts: DiskCounts,
    /// Bytes written to each file and not yet flushed: what a clone of it would force out.
    dirty: Vec<(FileName, u64)>,
    /// Flushes in flight: the tag, the file, and how many of its dirty bytes the flush covers.
    flushing: Vec<(Tag, FileName, u64)>,
    /// File reads in flight.
    reading: Vec<Tag>,
}

impl Disk {
    #[blaze::nonblocking]
    pub(crate) fn new() -> Disk {
        Disk {
            counts: DiskCounts::default(),
            // alloc: none until a file is written.
            dirty: Vec::new(),
            // alloc: none until a flush is asked.
            flushing: Vec::new(),
            // alloc: none until a file is read.
            reading: Vec::new(),
        }
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn counts(&self) -> DiskCounts {
        self.counts
    }

    /// A caller waited for the device: a flush or a name sync it did not return before.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn waited(&mut self) {
        self.counts.device_waits += 1;
    }

    fn dirty_of(&self, file: &FileName) -> u64 {
        self.dirty.iter().find(|(it, _)| it == file).map_or(0, |(_, bytes)| *bytes)
    }

    /// The dirty bytes of `file`, which it no longer has.
    fn take_dirty(&mut self, file: &FileName) -> u64 {
        match self.dirty.iter().position(|(it, _)| it == file) {
            Some(at) => self.dirty.swap_remove(at).1,
            None => 0,
        }
    }

    fn wrote(&mut self, file: &FileName, bytes: u64) {
        self.counts.written += bytes;
        match self.dirty.iter_mut().find(|(it, _)| it == file) {
            Some((_, dirty)) => *dirty += bytes,
            // clone: the ledger's name of the file, a refcount.
            None => self.dirty.push((file.clone(), bytes)),
        }
    }

    /// `from` renamed over `to`: a file's unflushed bytes go where it goes, and what `to`
    /// held unflushed is gone with it.
    fn moved(&mut self, from: &FileName, to: &FileName) {
        let moved = self.take_dirty(from);
        self.take_dirty(to);
        if moved > 0 {
            // clone: the ledger's name of the file, a refcount.
            self.dirty.push((to.clone(), moved));
        }
    }

    /// `effect` is handed to the driver now.
    pub(crate) fn submitted(&mut self, effect: &Effect) {
        match effect {
            Effect::Append { file, bytes, .. } | Effect::WriteAt { file, bytes, .. } => {
                self.wrote(file, bytes.len() as u64);
            }
            // Cut, or for a mapped write cut and written through the mapping: dirty pages the
            // kernel counts, not a write asked here.
            Effect::Truncate { file, .. } | Effect::MapWrite { file, .. } => self.wrote(file, 0),
            // blaze-core's whole-file write is durable when it completes.
            Effect::Write { file, bytes, .. } => {
                self.counts.written += bytes.len() as u64;
                self.take_dirty(file);
            }
            Effect::Sync { file, tag } => {
                self.counts.flushes += 1;
                let covered = self.dirty_of(file);
                // clone: the ledger's name of the file, a refcount.
                self.flushing.push((*tag, file.clone(), covered));
            }
            // A barrier writes out what a flush writes out and makes nothing durable by
            // itself: the simulation orders it before what is written after.
            Effect::Barrier { file, tag } => {
                self.counts.writebacks += 1;
                let covered = self.dirty_of(file);
                // clone: the ledger's name of the file, a refcount.
                self.flushing.push((*tag, file.clone(), covered));
            }
            // A write-back cleans what a flush cleans (a clone after it forces nothing out)
            // and makes nothing durable: the simulation still tears the file at a crash.
            Effect::WriteBack { file, tag } => {
                self.counts.writebacks += 1;
                let covered = self.dirty_of(file);
                // clone: the ledger's name of the file, a refcount.
                self.flushing.push((*tag, file.clone(), covered));
            }
            Effect::Read { tag, .. } | Effect::ReadAll { tag, .. } => self.reading.push(*tag),
            // An unsynced rename counts no device wait, which is the point of it.
            Effect::Rename { from, to, .. } | Effect::RenameUnsynced { from, to, .. } => {
                self.moved(from, to);
            }
            Effect::Remove { file, .. } => {
                self.take_dirty(file);
            }
            Effect::Clone { from, .. } => {
                self.counts.clones += 1;
                // The kernel writes a source's unflushed bytes out before it shares its blocks.
                let forced = self.take_dirty(from);
                self.counts.forced_writeback += forced;
            }
            // A copy of the data reads the source's pages where they are, written back or
            // not: it forces nothing out, as a clone does.
            Effect::Link { .. } | Effect::CopyData { .. } => {}
            // A prewarm asks the page cache to read ahead and is not waited for: no byte is
            // written, read for a caller or flushed, and no name changes.
            Effect::Prewarm { .. } => {}
            Effect::ReadInto { .. } | Effect::ReadMany { .. } | Effect::WriteFrom { .. } | Effect::ReadLink { .. } | Effect::SyncDir { .. } | Effect::WriteUnsynced { .. } | Effect::List { .. } | Effect::Mode { .. } | Effect::Seal { .. } | Effect::Private { .. } | Effect::Times { .. } | Effect::Stat { .. } | Effect::Lstat { .. } | Effect::Lock { .. } | Effect::Unlock { .. } | Effect::Watch { .. } | Effect::Journal { .. } | Effect::Unwatch { .. } | Effect::Stamp { .. } | Effect::Entries { .. } | Effect::Flush { .. } | Effect::Timer { .. } | Effect::Map { .. } | Effect::Spawn { .. } | Effect::Kill { .. } | Effect::Listen { .. } | Effect::Unlisten { .. } | Effect::Connect { .. } | Effect::Recv { .. } | Effect::Send { .. } | Effect::Hangup { .. } | Effect::Pressure { .. } | Effect::Bind { .. } | Effect::SendTo { .. } | Effect::RecvFrom { .. } | Effect::Unbind { .. } | Effect::Resolve { .. } | Effect::ReadStdin { .. } | Effect::WriteOut { .. } | Effect::Now { .. } | Effect::Random { .. } | Effect::Roots { .. } | Effect::MakeDir { .. } | Effect::RemoveDir { .. } | Effect::Ancestors { .. } | Effect::Above { .. } => {}
        }
    }

    /// `completion` was polled, for its waiter or for none.
    #[blaze::nonblocking]
    pub(crate) fn completed(&mut self, completion: &Completion) {
        let tag = completion.tag;
        if let Some(at) = self.flushing.iter().position(|(it, _, _)| *it == tag) {
            let (_, file, covered) = self.flushing.swap_remove(at);
            if matches!(completion.done, Done::Synced | Done::Ordered | Done::WrittenBack) {
                if let Some((_, dirty)) = self.dirty.iter_mut().find(|(it, _)| *it == file) {
                    // What was written after the flush was asked is still unflushed.
                    *dirty = dirty.saturating_sub(covered);
                }
                self.dirty.retain(|(_, dirty)| *dirty > 0);
            }
        }
        if let Some(at) = self.reading.iter().position(|it| *it == tag) {
            self.reading.swap_remove(at);
            if let Done::Read(bytes) = &completion.done {
                self.counts.read += bytes.len() as u64;
            }
        }
    }

    /// A directory's names synced: counted.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn names_synced(&mut self) {
        self.counts.name_syncs += 1;
    }

    /// The machine lost power, after the simulation tore each file's unflushed bytes and
    /// undid the name changes no directory sync covered: nothing is in flight and nothing is
    /// unflushed after.
    pub(crate) fn crashed(&mut self) {
        self.dirty.clear();
        self.flushing.clear();
        self.reading.clear();
    }
}
