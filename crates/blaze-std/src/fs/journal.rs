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
//! `caught_up`, then an [`Effect::Flush`] of that watch waited to its [`Done::Flushed`], then
//! an [`Effect::Unwatch`] of the same tag waited to its [`Done::Unwatched`]. Every batch heard
//! before the stream ended is kept, the live ones after the caught-up one too, and the cursor
//! is the last batch's: the names are a superset of what changed up to the cursor, never less.
//!
//! **Why the flush.** FSEvents tells its history, then `HistoryDone`, then what it hears live
//! (FSEvents.h: "your callback will be invoked with historical events, then a single event
//! with the kFSEventStreamEventFlagHistoryDone flag, then contemporary events"). A write made
//! just before this call whose event fseventsd had not yet put in its store when the history
//! was read comes as a contemporary event, after `HistoryDone`. Unflushed, this call would
//! return without it, and the request would answer from a file it believes unchanged: stale
//! once. The flush completes only after every change queued for the watch when it was asked
//! (blaze-core `Effect::Flush`; fsevents.rs writes a cookie file and waits to hear it, since
//! fseventsd reads the kernel's one queue in order), so every write made before this call is in
//! the returned names.
//!
//! Every wait is the task's ([`Cx::wait`], its deadline): a deadline that passes is the
//! refusal [`FsKind::Stopped`], with the watch ended.
//!
//! **Held.** [`Dir::hold_changes`] makes the same stream once and keeps it ([`HeldChanges`]):
//! the journal helper's, which answers commands' asks with one flush each and never a stream's
//! start, history and end (blaze-watch helper.rs). Its cursor follows the same rule.
//!
//! **What the cursor is** (the rule is stated once, blaze-core fsevents.rs `Journal`): the
//! highest event ID told, which after the flush is at least the cookie's, so every change made
//! before the flush is at or before it and was told here; a replay from it tells only the
//! changes after it. It is never the system's current ID read after the flush (an event of
//! this folder below that may not have been told yet).
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

/// What [`Dir::changes_since`] heard. The folder asked may be the driver's root (a checkout
/// run from its own folder): its own level is told as any other.
#[derive(Debug)]
pub struct Changes {
    /// The names strictly beneath the folder the journal said were made, written, removed or
    /// renamed, each once, sorted, under the driver's root.
    names: Vec<FileName>,
    /// The subtrees strictly beneath the folder whose changes the journal could not name,
    /// each with why.
    lost: Vec<(FileName, Lost)>,
    /// The folder itself lost, with why: the first heard ([`blaze_core::Journaled::whole`]).
    whole: Option<Lost>,
    /// The folder's own entry was told ([`blaze_core::Journaled::itself`]).
    itself: bool,
    /// Where, in a name under the driver's root, the part relative to the folder starts.
    start: usize,
    /// Where the journal stood once these were told: the cursor to save.
    pub cursor: Cursor,
    /// The names and lost subtrees told, over every batch, before they were merged.
    pub events: u64,
    /// What each stretch of the replay took ([`ReplayTimes`]).
    pub times: ReplayTimes,
}

/// What one replay's stretches took on the driver's clock (ns), each in a span of its own (rule
/// 15): the three waits, and the three effects handed to the driver, which a real driver performs
/// on the asking thread inside `Io::submit` before any wait begins (on FSEvents:
/// IPC to fseventsd, a file written, a stream stopped on its queue). Until 2026-11-21 those three
/// were in no span and no count: a 177 ms replay (tempo, score-c0230wave85 edit2) said "0 waits"
/// and nothing of where its time went. The profile's `replays:` part prints the longest replay's.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ReplayTimes {
    /// The journal handed over: on FSEvents (blaze-core fsevents.rs `Watch::journal`) the
    /// volume's store UUID and the current event ID asked of fseventsd, `FSEventStreamCreate`
    /// and `FSEventStreamStart`; a process's first watch also loads CoreServices and makes its
    /// cookies' folder (`Watch::new`).
    pub start_ns: u64,
    /// From the start handed over to the first batch heard (a history's first chunk read).
    pub first_ns: u64,
    /// From the start handed over to the batch with `HistoryDone`.
    pub history_ns: u64,
    /// The batches heard up to and with `HistoryDone`.
    pub history_batches: u32,
    /// The flush handed over: on FSEvents the cookie file made (`std::fs::write`, no sync).
    pub cookie_write_ns: u64,
    /// From the flush handed over to its cookie heard (`Done::Flushed`).
    pub cookie_heard_ns: u64,
    /// The unwatch handed over: on FSEvents `FSEventStreamStop` and `FSEventStreamInvalidate`
    /// run on the stream's queue (two `dispatch_sync`), and what it told moved to the driver.
    pub stop_ns: u64,
    /// From the unwatch handed over to its `Done::Unwatched` taken.
    pub end_ns: u64,
}

impl ReplayTimes {
    /// The six stretches summed: the replay's wall time as its parts saw it.
    pub fn total_ns(&self) -> u64 {
        self.start_ns
            .saturating_add(self.history_ns)
            .saturating_add(self.cookie_write_ns)
            .saturating_add(self.cookie_heard_ns)
            .saturating_add(self.stop_ns)
            .saturating_add(self.end_ns)
    }
}

impl core::fmt::Display for ReplayTimes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let ms = |ns: u64| ns as f64 / 1e6;
        write!(
            f,
            "stream started {:.1} ms, first batch {:.1} ms, HistoryDone {:.1} ms after {} batch(es), cookie written {:.1} ms, cookie heard {:.1} ms, stream stopped {:.1} ms, its end heard {:.1} ms",
            ms(self.start_ns),
            ms(self.first_ns),
            ms(self.history_ns),
            self.history_batches,
            ms(self.cookie_write_ns),
            ms(self.cookie_heard_ns),
            ms(self.stop_ns),
            ms(self.end_ns),
        )
    }
}

/// Nanoseconds from `from` to `to` on the driver's clock (0 if it went back).
fn ns_between(from: core::time::Duration, to: core::time::Duration) -> u64 {
    u64::try_from(to.saturating_sub(from).as_nanos()).unwrap_or(u64::MAX)
}

impl Changes {
    /// The changed names, relative to the folder asked, sorted by their bytes, each once:
    /// `""` (first) is the folder's own entry.
    pub fn files(&self) -> impl Iterator<Item = &str> + '_ {
        let itself = self.itself.then_some("");
        itself.into_iter().chain(self.names.iter().map(|name| relative(name, self.start)))
    }

    /// The subtrees lost, relative to the folder asked: `""` (first) is the folder itself.
    pub fn lost(&self) -> impl Iterator<Item = (&str, Lost)> + '_ {
        let whole = self.whole.map(|why| ("", why));
        whole.into_iter().chain(self.lost.iter().map(|(name, why)| (relative(name, self.start), *why)))
    }

    /// How many names and lost subtrees [`Changes::files`] and [`Changes::lost`] give.
    pub fn told(&self) -> usize {
        self.names.len() + self.lost.len() + usize::from(self.itself) + usize::from(self.whole.is_some())
    }

    /// Whether nothing changed: no name told and nothing lost.
    pub fn is_empty(&self) -> bool {
        self.told() == 0
    }
}

/// One batch kept in `changes`: its events counted, its cursor taken, its names and lost
/// subtrees kept. Once the folder is lost whole (told so, or past [`CHANGES_MOST`] names, the
/// cap's stated rule, as [`Lost::Dropped`]) nothing beneath it is kept: the caller sweeps it.
fn take(changes: &mut Changes, batch: blaze_core::Journaled) {
    let told = batch.files.len().saturating_add(batch.lost.len()) + usize::from(batch.itself) + usize::from(batch.whole.is_some());
    changes.events = changes.events.saturating_add(u64::try_from(told).unwrap_or(u64::MAX));
    changes.cursor = batch.cursor;
    changes.itself |= batch.itself;
    changes.whole = changes.whole.or(batch.whole);
    if changes.whole.is_none()
        && changes.names.len().saturating_add(batch.files.len()).saturating_add(changes.lost.len()).saturating_add(batch.lost.len())
            > CHANGES_MOST
    {
        changes.whole = Some(Lost::Dropped);
    }
    if changes.whole.is_some() {
        changes.names.clear();
        changes.lost.clear();
        return;
    }
    changes.names.extend(batch.files);
    changes.lost.extend(batch.lost);
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
    /// The journal was asked and failed, or the task stopped (its deadline, cancellation)
    /// while it was heard: the watch was ended.
    Failed(FsError),
}

impl core::fmt::Display for JournalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            JournalError::NoJournal { dir } => write!(f, "bstd::fs changes_since of {dir}: {}", Lost::NoJournal),
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
        let failed = |kind| JournalError::Failed(FsError::new(FsOp::Journal, &self.base, kind));
        cx.stopped_by(self.io.now()).map_err(|stopped| failed(FsKind::Stopped(stopped)))?;
        // clone: the effect's name, a refcount; the root too (a checkout run from its folder).
        let dir = self.base.clone();
        let mut times = ReplayTimes::default();
        // Each effect handed over and each of the three waits in a span of its own (rule 15), so
        // the profile says which of the start, the history, the cookie's write, the barrier, the
        // stop and the stream's end a replay's time went to ([`ReplayTimes`]).
        let began = self.io.now();
        let tag = {
            let _starting = cx.span("journal: the stream made and started (the effect handed over)");
            self.io.submit(|tag| Effect::Journal { dir, since, tag })
        };
        let started = self.io.now();
        times.start_ns = ns_between(began, started);
        // alloc: the replay's names, kept for the caller, capped at `CHANGES_MOST`.
        let mut changes = Changes {
            names: Vec::new(),
            lost: Vec::new(),
            whole: None,
            itself: false,
            start: self.base.start(),
            cursor: since,
            events: 0,
            times,
        };
        let history = cx.span("journal: the history told, to HistoryDone");
        loop {
            match history.wait(&self.io, tag, Ends::Watch) {
                Ok(Done::Journaled(batch)) => {
                    let caught_up = batch.caught_up;
                    let at = ns_between(started, self.io.now());
                    if changes.times.history_batches == 0 {
                        changes.times.first_ns = at;
                    }
                    changes.times.history_batches = changes.times.history_batches.saturating_add(1);
                    take(&mut changes, *batch);
                    if caught_up {
                        changes.times.history_ns = at;
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
        // The barrier (the module's doc, "Why the flush"): every change queued for the watch
        // by now is told before `Flushed`, kept as the history was.
        let writing = self.io.now();
        let flush = {
            let _writing = cx.span("journal: the flush's cookie written (the effect handed over)");
            self.io.submit(|fresh| Effect::Flush { watch: tag, tag: fresh })
        };
        let written = self.io.now();
        changes.times.cookie_write_ns = ns_between(writing, written);
        let barrier = cx.span("journal: the flush's cookie heard (the barrier)");
        match barrier.wait(&self.io, flush, Ends::Once) {
            Ok(Done::Flushed) => changes.times.cookie_heard_ns = ns_between(written, self.io.now()),
            Ok(done) => {
                self.abandon(tag);
                return Err(JournalError::Failed(unexpected(FsOp::Journal, &self.base, done)));
            }
            Err(stopped) => {
                self.abandon(tag);
                return Err(failed(FsKind::Stopped(stopped)));
            }
        }
        // The watch ended: an Unwatch of the same tag, its `Unwatched` the stream's last. The
        // batches before it (those the flush ordered, and any live one after) are kept: more
        // names than changed since the cursor is a row statted, never one missed.
        let stopping = self.io.now();
        {
            let _stopping = cx.span("journal: the stream stopped (the effect handed over)");
            let _same = self.io.submit(|_fresh| Effect::Unwatch { tag });
        }
        let stopped = self.io.now();
        changes.times.stop_ns = ns_between(stopping, stopped);
        let ending = cx.span("journal: the stream ended");
        loop {
            match ending.wait(&self.io, tag, Ends::Watch) {
                Ok(Done::Unwatched) => {
                    changes.times.end_ns = ns_between(stopped, self.io.now());
                    break;
                }
                Ok(Done::Journaled(live)) => take(&mut changes, *live),
                Ok(done) => return Err(JournalError::Failed(unexpected(FsOp::Journal, &self.base, done))),
                Err(stopped) => {
                    self.io.forget(tag, Ends::Watch);
                    return Err(failed(FsKind::Stopped(stopped)));
                }
            }
        }
        changes.names.sort_unstable();
        changes.names.dedup();
        Ok(changes)
    }

    /// A stream given up before its end: the driver is told to end it, and whatever it still
    /// brings for `tag` is dropped as it arrives.
    fn abandon(&self, tag: Tag) {
        let _same = self.io.submit(|_fresh| Effect::Unwatch { tag });
        self.io.forget(tag, Ends::Watch);
    }

    /// No names yet, at `cursor`: a [`Changes`] for this folder to keep batches in.
    fn none_at(&self, cursor: Cursor) -> Changes {
        // alloc: none until a batch names something.
        Changes { names: Vec::new(), lost: Vec::new(), whole: None, itself: false, start: self.base.start(), cursor, events: 0, times: ReplayTimes::default() }
    }

    /// A journal stream of this folder from now, held open: what [`Dir::changes_since`] does
    /// once per call, the journal helper does once per life (blaze-watch helper.rs; blaze-cli
    /// journal_helper.rs), so a command's ask costs one flush of a stream already in place and
    /// never a stream's start, its history and its end (tempo, c0305wave88: the delivery that
    /// carries `HistoryDone` waits ~160 ms in fseventsd on one replay in two, "HistoryDone
    /// 162.6 ms after 2 batch(es)"). Returned once the stream has caught up (its history from
    /// the current ID, empty), under `cx`'s deadline; what it tells from there is taken by
    /// [`HeldChanges::take`] and [`HeldChanges::flush`].
    pub fn hold_changes(&self, cx: &Cx) -> Result<HeldChanges<'_, P>, JournalError> {
        if !self.io.keeps_journal() {
            // clone: an error's name of its folder, a refcount.
            return Err(JournalError::NoJournal { dir: self.base.clone() });
        }
        let failed = |kind| JournalError::Failed(FsError::new(FsOp::Journal, &self.base, kind));
        cx.stopped_by(self.io.now()).map_err(|stopped| failed(FsKind::Stopped(stopped)))?;
        // clone: the effect's name, a refcount.
        let dir = self.base.clone();
        let tag = {
            let _starting = cx.span("journal: the held stream made and started (the effect handed over)");
            self.io.submit(|tag| Effect::Journal { dir, since: Cursor::None, tag })
        };
        let mut held = HeldChanges { dir: self, tag, cursor: Cursor::None, open: true };
        let mut first = self.none_at(Cursor::None);
        let history = cx.span("journal: the held stream in place, to HistoryDone");
        loop {
            match history.wait(&self.io, tag, Ends::Watch) {
                Ok(Done::Journaled(batch)) => {
                    let caught_up = batch.caught_up;
                    take(&mut first, *batch);
                    if caught_up {
                        break;
                    }
                }
                Ok(done) => return Err(held.failed(done)),
                Err(stopped) => return Err(failed(FsKind::Stopped(stopped))),
            }
        }
        // What came before the stream caught up is before any place the holder issues: only the
        // cursor is kept.
        held.cursor = first.cursor;
        Ok(held)
    }
}

/// A journal stream held open by [`Dir::hold_changes`]: its batches taken as they come
/// ([`HeldChanges::take`]) or behind a barrier ([`HeldChanges::flush`]). Dropped open, it is
/// ended and whatever it still brings dropped; [`HeldChanges::end`] waits for its end.
pub struct HeldChanges<'d, P: Rights> {
    dir: &'d Dir<P>,
    tag: Tag,
    /// The last batch's cursor: every change told so far is at or before it.
    cursor: Cursor,
    /// The stream is the driver's still: dropping it unwatches it.
    open: bool,
}

impl<P: Rights> HeldChanges<'_, P> {
    /// Where the stream has read to: the cursor of the last batch taken.
    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// What the stream told since the last take, without waiting: the batches the driver has
    /// brought by now. A batch of the folder lost (`whole`) is told as any other; the holder
    /// decides what a loss ends.
    pub fn take(&mut self, cx: &Cx) -> Result<Changes, JournalError> {
        let failed = |kind| JournalError::Failed(FsError::new(FsOp::Journal, &self.dir.base, kind));
        cx.stopped_by(self.dir.io.now()).map_err(|stopped| failed(FsKind::Stopped(stopped)))?;
        let mut changes = self.dir.none_at(self.cursor);
        self.drain(&mut changes)?;
        Ok(self.sorted(changes))
    }

    /// Every change made before this call, told: a flush of the stream (the module's doc, "Why
    /// the flush") waited to its [`Done::Flushed`] under `cx`'s deadline, then every batch
    /// before it taken. The returned cursor is the last batch's, at or after the flush's cookie:
    /// a place issued now and this cursor are one moment (blaze-watch helper.rs `JournalAt`).
    pub fn flush(&mut self, cx: &Cx) -> Result<Changes, JournalError> {
        let failed = |kind| JournalError::Failed(FsError::new(FsOp::Journal, &self.dir.base, kind));
        cx.stopped_by(self.dir.io.now()).map_err(|stopped| failed(FsKind::Stopped(stopped)))?;
        let mut changes = self.dir.none_at(self.cursor);
        let watch = self.tag;
        let writing = self.dir.io.now();
        let flush = {
            let _writing = cx.span("journal: the held stream's cookie written (the effect handed over)");
            self.dir.io.submit(|fresh| Effect::Flush { watch, tag: fresh })
        };
        let written = self.dir.io.now();
        changes.times.cookie_write_ns = ns_between(writing, written);
        let barrier = cx.span("journal: the held stream's cookie heard (the barrier)");
        match barrier.wait(&self.dir.io, flush, Ends::Once) {
            Ok(Done::Flushed) => changes.times.cookie_heard_ns = ns_between(written, self.dir.io.now()),
            Ok(done) => {
                let error = unexpected(FsOp::Journal, &self.dir.base, done);
                self.end_now();
                return Err(JournalError::Failed(error));
            }
            // The stream is kept: a flush given up at its deadline ends no watch, and the next
            // flush is a barrier of its own.
            Err(stopped) => return Err(failed(FsKind::Stopped(stopped))),
        }
        // The driver tells a batch before the flush its events order (fsevents.rs `journaled`
        // before `tell_flushes`, both on the stream's one queue), so every batch before the
        // cookie was polled with or before `Flushed`, and is taken here.
        self.drain(&mut changes)?;
        Ok(self.sorted(changes))
    }

    /// The stream ended, its end waited to its [`Done::Unwatched`] under `cx`'s deadline.
    pub fn end(mut self, cx: &Cx) -> Result<(), JournalError> {
        let tag = self.tag;
        self.open = false;
        let _same = self.dir.io.submit(|_fresh| Effect::Unwatch { tag });
        let ending = cx.span("journal: the held stream ended");
        loop {
            match ending.wait(&self.dir.io, tag, Ends::Watch) {
                Ok(Done::Unwatched) => return Ok(()),
                Ok(Done::Journaled(_)) => {}
                Ok(done) => return Err(JournalError::Failed(unexpected(FsOp::Journal, &self.dir.base, done))),
                Err(stopped) => {
                    self.dir.io.forget(tag, Ends::Watch);
                    return Err(JournalError::Failed(FsError::new(FsOp::Journal, &self.dir.base, FsKind::Stopped(stopped))));
                }
            }
        }
    }

    /// Every batch the driver has brought for the stream kept in `changes`, polling without
    /// waiting until a poll brings nothing.
    fn drain(&mut self, changes: &mut Changes) -> Result<(), JournalError> {
        loop {
            while let Some(done) = self.dir.io.take(self.tag) {
                match done {
                    Done::Journaled(batch) => {
                        self.cursor = batch.cursor;
                        take(changes, *batch);
                    }
                    done => return Err(self.failed(done)),
                }
            }
            if self.dir.io.poll(blaze_core::wait::Deadline::PASSED) == 0 {
                return Ok(());
            }
        }
    }

    /// The names each once, sorted, as [`Dir::changes_since`] gives them.
    fn sorted(&self, mut changes: Changes) -> Changes {
        changes.names.sort_unstable();
        changes.names.dedup();
        changes
    }

    /// The stream told `done`, which is no batch: a failure ends it (nothing left to
    /// unwatch); anything else is ended here.
    fn failed(&mut self, done: Done) -> JournalError {
        match done {
            Done::Failed(_) => self.open = false,
            _ => self.end_now(),
        }
        JournalError::Failed(unexpected(FsOp::Journal, &self.dir.base, done))
    }

    /// The stream ended without waiting: what it still brings is dropped as it arrives.
    fn end_now(&mut self) {
        if self.open {
            self.open = false;
            self.dir.abandon(self.tag);
        }
    }
}

impl<P: Rights> Drop for HeldChanges<'_, P> {
    fn drop(&mut self) {
        self.end_now();
    }
}
