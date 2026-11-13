//! The simulated disk's ledger (docs/v2/plan/design/std.md, "Files: whether bytes reached the
//! disk is in the type"): what blaze-core's simulation does not model and a test of file code
//! must be able to assert.
//!
//! - **What was paid.** [`DiskCounts`]: bytes written and read, flushes asked, the flushes and
//!   name syncs a caller waited for, clones, and the bytes a clone forced out because its
//!   source was not flushed (`forced_writeback`). A test of an edit path asserts the last is
//!   zero, with no real disk.
//! - **Which names a crash may undo.** blaze-core's simulation keeps every completed rename,
//!   remove and create through a crash. On a real disk a name is durable only once its
//!   directory is synced, so the ledger keeps each name change made since the last name sync
//!   that covers it, oldest first, and [`Disk::crashed`] undoes a seeded suffix of them, as a
//!   journal that had committed only a prefix would.
//!
//! What the model keeps simpler than a disk: a name sync never fails; a file a rename or a
//! remove replaced comes back whole when a crash undoes that change, its unflushed bytes
//! included (a real disk could tear them too); and a name change is as its effect saw the
//! disk when it was handed over, so two changes of one name in flight at once are not
//! ordered against each other.
//!
//! The ledger exists only in a simulated world ([`crate::Io::disk`] is `None` on a real
//! driver). It reads every effect as it is handed to the driver and every completion as it is
//! polled, so no file call can forget to tell it.
//!
//! **The model.** A name sync of a directory makes durable every name change in it and every
//! name change made before the last of those, in any directory (a journal commits in order);
//! a crash keeps a prefix of the changes after and undoes the rest, newest first.

use alloc::{alloc::Global, vec::Vec as HeapVec};
use blaze_core::{Completion, Done, Effect, FileName, Tag, sim::Sim};
use blaze_rng::SplitMix64;

use crate::{Stopped, mem::Vec};

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

/// A change to a directory's names that no name sync covers yet.
#[derive(Debug)]
enum NameChange {
    /// `name` came to be.
    Created { name: FileName },
    /// `from` was renamed over `to`, which held `before` (or was not there).
    Renamed { from: FileName, to: FileName, before: Option<HeapVec<u8>> },
    /// `name`, which held `before`, was removed.
    Removed { name: FileName, before: HeapVec<u8> },
}

impl NameChange {
    /// The name whose directory a sync must cover for this change to be durable.
    fn name(&self) -> &FileName {
        match self {
            NameChange::Created { name } | NameChange::Removed { name, .. } => name,
            NameChange::Renamed { to, .. } => to,
        }
    }
}

/// The directory `name` is in: everything before its last `/`, or the root's empty name.
pub(crate) fn dir_of(name: &FileName) -> &str {
    match name.as_str().rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// What the ledger's own choices (which name changes a crash keeps) are drawn from, besides
/// the world's seed.
const DISK_SALT: u64 = 0x6273_7464_2e64_736b;

/// The ledger of one simulated world. Its lists are on the global heap and grow fallibly: a
/// growth with no room is [`Stopped::OutOfMemory`] from the call that grew it, with the
/// ledger as it was before that call (each call reserves what it adds before it changes
/// anything).
pub(crate) struct Disk {
    counts: DiskCounts,
    /// Bytes written to each file and not yet flushed: what a clone of it would force out.
    dirty: Vec<(FileName, u64), Global>,
    /// Name changes no name sync covers yet, oldest first.
    names: Vec<NameChange, Global>,
    /// Name changes whose effect is in flight: one joins `names` when its effect completes
    /// having done it, and is dropped when the effect fails or a crash loses it.
    naming: Vec<(Tag, NameChange), Global>,
    /// Flushes in flight: the tag, the file, and how many of its dirty bytes the flush covers.
    flushing: Vec<(Tag, FileName, u64), Global>,
    /// File reads in flight.
    reading: Vec<Tag, Global>,
    rng: SplitMix64,
}

impl Disk {
    pub(crate) fn new(seed: u64) -> Disk {
        Disk {
            counts: DiskCounts::default(),
            // alloc: none until a file is written; the simulated world's ledger lives as long
            // as the world.
            dirty: Vec::new_in(Global),
            // alloc: none until a name changes.
            names: Vec::new_in(Global),
            // alloc: none until a name changes.
            naming: Vec::new_in(Global),
            // alloc: none until a flush is asked.
            flushing: Vec::new_in(Global),
            // alloc: none until a file is read.
            reading: Vec::new_in(Global),
            rng: SplitMix64::new(seed ^ DISK_SALT),
        }
    }

    pub(crate) fn counts(&self) -> DiskCounts {
        self.counts
    }

    /// A caller waited for the device: a flush or a name sync it did not return before.
    pub(crate) fn waited(&mut self) {
        self.counts.device_waits += 1;
    }

    fn dirty_of(&self, file: &FileName) -> u64 {
        self.dirty.iter().find(|(it, _)| it == file).map_or(0, |(_, bytes)| *bytes)
    }

    /// The dirty bytes of `file`, which it no longer has: none when it had none.
    fn take_dirty(&mut self, file: &FileName) -> u64 {
        let Some(at) = self.dirty.iter().position(|(it, _)| it == file) else { return 0 };
        self.dirty.swap_remove(at).map_or(0, |(_, bytes)| bytes)
    }

    /// `bytes` written to `file`. With no room for a new file's entry nothing is counted.
    fn wrote(&mut self, file: &FileName, bytes: u64) -> Result<(), Stopped> {
        match self.dirty.iter_mut().find(|(it, _)| it == file) {
            Some((_, dirty)) => *dirty += bytes,
            // clone: the ledger's name of the file, a refcount.
            None => self.dirty.push((file.clone(), bytes))?,
        }
        self.counts.written += bytes;
        Ok(())
    }

    /// The effect of `tag` makes `file`, if it is not there: a name no sync covers, once
    /// the effect has completed.
    fn made(&mut self, sim: &Sim, tag: Tag, file: &FileName) -> Result<(), Stopped> {
        if !sim.exists(file) {
            // clone: the ledger's name of the file, a refcount.
            self.naming.push((tag, NameChange::Created { name: file.clone() }))?;
        }
        Ok(())
    }

    /// `effect` is handed to the driver now. With no room in the ledger for what it records,
    /// [`Stopped::OutOfMemory`] and the ledger as it was: room for every entry the effect may
    /// add (at most one in each list) is reserved before anything is recorded, so no push
    /// below can fail halfway.
    pub(crate) fn submitted(&mut self, sim: &Sim, effect: &Effect) -> Result<(), Stopped> {
        // The entries the effect may add: (dirty, naming, flushing, reading).
        let (dirty, naming, flushing, reading) = match effect {
            Effect::Append { .. } | Effect::Truncate { .. } | Effect::Rename { .. } => (1, 1, 0, 0),
            Effect::WriteAt { .. } => (1, 0, 0, 0),
            Effect::Sync { .. } | Effect::WriteBack { .. } => (0, 0, 1, 0),
            Effect::Read { .. } | Effect::ReadAll { .. } => (0, 0, 0, 1),
            Effect::Remove { .. } | Effect::Clone { .. } | Effect::Link { .. } => (0, 1, 0, 0),
            _ => (0, 0, 0, 0),
        };
        self.dirty.reserve(dirty)?;
        self.naming.reserve(naming)?;
        self.flushing.reserve(flushing)?;
        self.reading.reserve(reading)?;
        match effect {
            Effect::Append { file, bytes, tag } => {
                self.made(sim, *tag, file)?;
                self.wrote(file, bytes.len() as u64)?;
            }
            Effect::WriteAt { file, bytes, .. } => self.wrote(file, bytes.len() as u64)?,
            Effect::Truncate { file, tag, .. } => {
                self.made(sim, *tag, file)?;
                self.wrote(file, 0)?;
            }
            // blaze-core's whole-file write is durable when it completes, its name too.
            Effect::Write { file, bytes, .. } => {
                self.counts.written += bytes.len() as u64;
                self.take_dirty(file);
                self.names.retain(|change| change.name() != file);
            }
            Effect::Sync { file, tag } => {
                self.counts.flushes += 1;
                let covered = self.dirty_of(file);
                // clone: the ledger's name of the file, a refcount.
                self.flushing.push((*tag, file.clone(), covered))?;
            }
            // A write-back cleans what a flush cleans (a clone after it forces nothing out)
            // and makes nothing durable: the simulation still tears the file at a crash.
            Effect::WriteBack { file, tag } => {
                self.counts.writebacks += 1;
                let covered = self.dirty_of(file);
                // clone: the ledger's name of the file, a refcount.
                self.flushing.push((*tag, file.clone(), covered))?;
            }
            Effect::Read { tag, .. } | Effect::ReadAll { tag, .. } => self.reading.push(*tag)?,
            Effect::Rename { from, to, tag } => {
                // alloc: a simulated crash's undo, the bytes the rename replaces, which
                // blaze-core's simulation copies out infallibly.
                let before = sim.exists(to).then(|| sim.contents(to));
                // clone: the ledger's names of the files, refcounts.
                self.naming.push((*tag, NameChange::Renamed { from: from.clone(), to: to.clone(), before }))?;
                // A file's unflushed bytes go where it goes: the entry `from` had, taken out
                // first, is the room the one for `to` is put back in.
                let moved = self.take_dirty(from);
                self.take_dirty(to);
                if moved > 0 {
                    // clone: the ledger's name of the file, a refcount.
                    self.dirty.push((to.clone(), moved))?;
                }
            }
            Effect::Remove { file, tag } => {
                if sim.exists(file) {
                    // alloc: a simulated crash's undo, the bytes the remove drops, which
                    // blaze-core's simulation copies out infallibly.
                    let before = sim.contents(file);
                    // clone: the ledger's name of the file, a refcount.
                    self.naming.push((*tag, NameChange::Removed { name: file.clone(), before }))?;
                }
                self.take_dirty(file);
            }
            Effect::Clone { from, to, tag } => {
                // The kernel writes a source's unflushed bytes out before it shares its blocks.
                self.made(sim, *tag, to)?;
                self.counts.clones += 1;
                let forced = self.take_dirty(from);
                self.counts.forced_writeback += forced;
            }
            Effect::Link { to, tag, .. } => self.made(sim, *tag, to)?,
            _ => {}
        }
        Ok(())
    }

    /// `completion` was polled, for its waiter or for none. With no room for the name change
    /// it completes, [`Stopped::OutOfMemory`] and the ledger as it was.
    pub(crate) fn completed(&mut self, completion: &Completion) -> Result<(), Stopped> {
        let tag = completion.tag;
        if let Some(at) = self.naming.iter().position(|(it, _)| *it == tag) {
            // An effect that failed changed no name.
            let done = !matches!(completion.done, Done::Failed(_));
            if done {
                self.names.reserve(1)?;
            }
            if let Some((_, change)) = self.naming.remove(at)
                && done
            {
                self.names.push(change)?;
            }
        }
        if let Some(at) = self.flushing.iter().position(|(it, _, _)| *it == tag)
            && let Some((_, file, covered)) = self.flushing.swap_remove(at)
        {
            if matches!(completion.done, Done::Synced | Done::WrittenBack) {
                if let Some((_, dirty)) = self.dirty.iter_mut().find(|(it, _)| *it == file) {
                    // What was written after the flush was asked is still unflushed.
                    *dirty = dirty.saturating_sub(covered);
                }
                self.dirty.retain(|(_, dirty)| *dirty > 0);
            }
        }
        if let Some(at) = self.reading.iter().position(|it| *it == tag)
            && self.reading.swap_remove(at).is_some()
            && let Done::Read(bytes) = &completion.done
        {
            self.counts.read += bytes.len() as u64;
        }
        Ok(())
    }

    /// The names of the directory `dir` synced: every change in it, and every change before
    /// the last of those, is durable.
    pub(crate) fn sync_names(&mut self, dir: &str) {
        self.counts.name_syncs += 1;
        if let Some(last) = self.names.iter().rposition(|change| dir_of(change.name()) == dir) {
            // The changes up to `last`, oldest first, are durable now: dropped from the list.
            let mut at = 0;
            self.names.retain(|_| {
                let keep = at > last;
                at += 1;
                keep
            });
        }
    }

    /// The machine lost power, after the simulation tore each file's unflushed bytes: a
    /// seeded prefix of the name changes no sync covered is kept, and the rest undone, newest
    /// first. Nothing is in flight and nothing is unflushed after.
    pub(crate) fn crashed(&mut self, sim: &mut Sim) {
        let kept = self.rng.below(self.names.len() as u64 + 1) as usize;
        // Newest first, down to the `kept` oldest.
        while self.names.len() > kept {
            let Some(change) = self.names.pop() else { break };
            match change {
                NameChange::Created { name } => sim.delete(&name),
                NameChange::Renamed { from, to, before } => {
                    sim.rename(&to, &from);
                    if let Some(before) = before {
                        sim.change(&to, &before);
                    }
                }
                NameChange::Removed { name, before } => sim.change(&name, &before),
            }
        }
        self.names.clear();
        // A name change whose effect was in flight was lost with it.
        self.naming.clear();
        self.dirty.clear();
        self.flushing.clear();
        self.reading.clear();
    }
}
