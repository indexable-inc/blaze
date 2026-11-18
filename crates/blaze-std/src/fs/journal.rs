//! What changed beneath a folder since a saved place in the system's journal of changes, asked
//! by a command that answers and exits, with no process alive in between (rule 70; plan row
//! S1). macOS keeps FSEvents' per-volume store across restarts and NTFS its USN journal: a
//! replay from a saved [`Cursor`] names every file made, written, removed or renamed since,
//! or says which subtree it cannot name ([`Lost`]). Linux keeps none: there the call is
//! refused BEFORE any effect is made ([`JournalError::NoJournal`]), since starting a journal
//! on Linux costs a watch's start (an inotify walk of every folder) only to be told that
//! everything since the cursor is lost.
//!
//! One call is one [`Effect::Journal`] from `since`, its batches taken until the one that is
//! `caught_up`, then an [`Effect::Unwatch`] of the same tag waited to its
//! [`Done::Unwatched`]. Batches heard after the caught-up one are live changes made after the
//! returned cursor: they are dropped here, and the next replay from that cursor names them.
//! Every wait is the task's ([`Cx::wait`], its deadline): a deadline that passes is the
//! refusal [`FsKind::Stopped`], with the watch ended.
//!
//! **The cursor is taken before the files are read.** The returned [`Changes::cursor`] is
//! where the journal stood when the replay caught up, so a caller that asks for changes FIRST
//! and reads its files AFTER saves a cursor no later than its reads: a file written between
//! the two has an event after the cursor, and the next replay names it. Asked in the other
//! order, that write would be before the cursor and after the read, and never seen.

use blaze_core::{Cursor, DirName, Done, Effect, FileName, Lost, Tag};

use super::{Dir, FsError, FsKind, FsOp, Rights, Vec, unexpected};
use crate::{cx::Cx, io::Ends};

/// The most names one replay keeps: past it the folder is taken as lost whole
/// ([`Lost::Dropped`] on the folder itself), so a replay after a large checkout or a rebase
/// costs one sweep, never an unbounded list.
pub const CHANGES_MOST: usize = 1 << 16;

/// What [`Dir::changes_since`] heard.
#[derive(Debug)]
pub struct Changes {
    /// The names the journal said were made, written, removed or renamed, each once, sorted,
    /// under the driver's root.
    names: Vec<FileName>,
    /// The subtrees whose changes the journal could not name, each with why.
    lost: Vec<(FileName, Lost)>,
    /// Where, in a name under the driver's root, the part relative to the folder starts.
    start: usize,
    /// Where the journal stood once these were told: the cursor to save.
    pub cursor: Cursor,
    /// The names and lost subtrees told, over every batch, before they were merged.
    pub events: u64,
}

impl Changes {
    /// The changed names, relative to the folder asked, sorted by their bytes, each once.
    pub fn files(&self) -> impl ExactSizeIterator<Item = &str> + '_ {
        self.names.iter().map(|name| relative(name, self.start))
    }

    /// The subtrees lost, relative to the folder asked: `""` is the folder itself.
    pub fn lost(&self) -> impl ExactSizeIterator<Item = (&str, Lost)> + '_ {
        self.lost.iter().map(|(name, why)| (relative(name, self.start), *why))
    }

    /// Whether nothing changed: no name told and nothing lost.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.lost.is_empty()
    }
}

/// `name` relative to a folder whose relative part starts at `start`: `""` for the folder.
fn relative(name: &FileName, start: usize) -> &str {
    name.as_str().get(start..).unwrap_or("")
}

/// Why [`Dir::changes_since`] said nothing of what changed.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum JournalError {
    /// The system keeps no journal of changes across a restart (Linux, a hosted program, a
    /// simulation playing [`blaze_core::sim::Journal::None`]): no effect was made.
    NoJournal { dir: DirName },
    /// The folder is the driver's root, which a journal effect cannot name: the caller
    /// journals the folders beneath it instead.
    Root,
    /// The journal was asked and failed, or the task stopped (its deadline, cancellation)
    /// while it was heard: the watch was ended.
    Failed(FsError),
}

impl core::fmt::Display for JournalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            JournalError::NoJournal { dir } => write!(f, "bstd::fs changes_since of {dir}: {}", Lost::NoJournal),
            JournalError::Root => f.write_str("bstd::fs changes_since of .: the driver's root has no name a journal can watch"),
            JournalError::Failed(error) => write!(f, "{error}"),
        }
    }
}

impl core::error::Error for JournalError {}

impl<P: Rights> Dir<P> {
    /// What changed beneath this folder since `since` ([`Cursor::None`]: nothing is told,
    /// and the returned cursor is where the journal is now), from the system's journal of
    /// changes (the module's doc). A read: any rights may ask it.
    pub fn changes_since(&self, cx: &Cx, since: Cursor) -> Result<Changes, JournalError> {
        if !self.io.keeps_journal() {
            // clone: an error's name of its folder, a refcount.
            return Err(JournalError::NoJournal { dir: self.base.clone() });
        }
        let DirName::Named(dir) = &self.base else { return Err(JournalError::Root) };
        let failed = |kind| JournalError::Failed(FsError::new(FsOp::Journal, &self.base, kind));
        cx.stopped_by(self.io.now()).map_err(|stopped| failed(FsKind::Stopped(stopped)))?;
        // clone: the effect's name, a refcount.
        let dir = dir.clone();
        let tag = self.io.submit(|tag| Effect::Journal { dir, since, tag });
        // alloc: the replay's names, kept for the caller, capped at `CHANGES_MOST`.
        let mut changes = Changes { names: Vec::new(), lost: Vec::new(), start: self.base.start(), cursor: since, events: 0 };
        loop {
            match cx.wait(&self.io, tag, Ends::Watch) {
                Ok(Done::Journaled(batch)) => {
                    let batch = *batch;
                    let told = batch.files.len().saturating_add(batch.lost.len());
                    changes.events = changes.events.saturating_add(u64::try_from(told).unwrap_or(u64::MAX));
                    changes.cursor = batch.cursor;
                    self.take(&mut changes, batch.files, batch.lost);
                    if batch.caught_up {
                        break;
                    }
                }
                // A failure ends the stream: nothing is left to unwatch.
                Ok(done @ Done::Failed(_)) => return Err(JournalError::Failed(unexpected(FsOp::Journal, &self.base, done))),
                Ok(done) => {
                    self.abandon(tag);
                    return Err(JournalError::Failed(unexpected(FsOp::Journal, &self.base, done)));
                }
                Err(stopped) => {
                    self.abandon(tag);
                    return Err(failed(FsKind::Stopped(stopped)));
                }
            }
        }
        changes.names.sort_unstable();
        changes.names.dedup();
        // The watch ended: an Unwatch of the same tag, its `Unwatched` the stream's last.
        let _same = self.io.submit(|_fresh| Effect::Unwatch { tag });
        loop {
            match cx.wait(&self.io, tag, Ends::Watch) {
                Ok(Done::Unwatched) => return Ok(changes),
                // A live batch, after the caught-up one: its changes are after the returned
                // cursor, so the next replay from it names them.
                Ok(Done::Journaled(_live)) => {}
                Ok(done) => return Err(JournalError::Failed(unexpected(FsOp::Journal, &self.base, done))),
                Err(stopped) => {
                    self.io.forget(tag, Ends::Watch);
                    return Err(failed(FsKind::Stopped(stopped)));
                }
            }
        }
    }

    /// One batch's names and lost subtrees kept in `changes`; past [`CHANGES_MOST`] the
    /// folder is lost whole instead (the cap's stated rule).
    fn take(&self, changes: &mut Changes, files: Vec<FileName>, lost: Vec<(FileName, Lost)>) {
        let whole = changes.lost.iter().any(|(name, _)| name.as_str().len() < changes.start);
        if whole {
            return;
        }
        if changes.names.len().saturating_add(files.len()).saturating_add(changes.lost.len()).saturating_add(lost.len()) > CHANGES_MOST {
            let DirName::Named(dir) = &self.base else { return };
            changes.names.clear();
            changes.lost.clear();
            // clone: the folder's name, a refcount.
            changes.lost.push((dir.clone(), Lost::Dropped));
            return;
        }
        changes.names.extend(files);
        changes.lost.extend(lost);
    }

    /// A stream given up before its end: the driver is told to end it, and whatever it still
    /// brings for `tag` is dropped as it arrives.
    fn abandon(&self, tag: Tag) {
        let _same = self.io.submit(|_fresh| Effect::Unwatch { tag });
        self.io.forget(tag, Ends::Watch);
    }
}
