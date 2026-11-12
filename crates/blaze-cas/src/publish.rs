//! The store's files outside its packs, and how bytes get to a path: whether a file's data
//! reached the disk is in its type, and how far a write goes toward the disk is an argument.
//!
//! Measured 2026-11-10 (xfs, a 162 MB executable): a whole-file write with no sync, 29 ms; a
//! clone (`FICLONE`) of a file written back, 1 ms; a clone of the file just written, 1.3 s,
//! because a file system that shares blocks writes the source's dirty pages out first and
//! waits for them. "Write it, then clone it" cost the sync it had been written to avoid, three
//! times an edit. So here that sequence cannot be written:
//!
//! - A file the store wrote is a [`Written`] until its write-back is known complete
//!   ([`blaze_core::Effect::WriteBack`]'s completion), then a [`Flushed`]. A clone is asked
//!   only through [`Flushed::clone_to`]; nothing else in the store makes an
//!   [`Effect::Clone`]. The bytes the store holds in its records are written wherever they
//!   are wanted whatever the file's state ([`write`]): a copy shares no block and reads no
//!   file, so nothing of the file's dirty data is forced out by it.
//! - Every write and rename made here takes a [`Durable`], with no default ([`write`],
//!   [`rename`]).
//! - [`crate::Store::publish`] is the one entry that makes a path hold some [`Content`]: it
//!   picks a write, a clone or a derive (a clone of a base and its ranges written) from what
//!   the content is, and its place's span says which (`how`). [`crate::Store::place`] is a
//!   caller of it.
//! - An output the build that asks just made is placed from the bytes its maker holds
//!   ([`crate::Store::place_made`], [`Content::Sparse`], [`Writing`]): the runs of the file
//!   that hold bytes written into a file of holes under a staged name, renamed onto the path.
//!   No file of the store is read, cloned or waited for, so the answer of an edit is held by
//!   no clone and no write-back, whatever the store's own files are doing
//!   (docs/v2/plan/design/clone-out.md, "An output the build made").
//! - The witness is in the types: a [`Flushed`] is made only from a [`Written`] and the
//!   completion of its write-back ([`Written::flushed`] takes the [`Done`] and gives the
//!   file back unless it is [`Done::WrittenBack`]), or of a file the store's commit synced.
//!
//! This is bstd's file API (docs/v2/plan/design/std.md, "Files: whether bytes reached the disk
//! is in the type") as far as a sans-IO core has it, to move there unchanged in what it
//! promises. Where it differs, and why: the store is a state machine on effects, so `flush`
//! gives the effect and [`Written::flushed`] is called with its completion, where bstd's
//! `written.flush(cx)` is one call that waits; a copy is [`write`] of bytes the caller
//! gives, on no handle, where bstd has `copy_to` on both, because the store keeps a value's
//! bytes in its records and never reads a whole file back to copy it; and a hard link is never chosen, since an output must be replaceable and writable by its
//! user without the store's file changing, and a link is one inode, mode and all.

#![forbid(unsafe_code)]

use blaze_core::{Done, Durable, Effect, FileName, Tag};
use blaze_ids::ContentId;

use crate::Source;

/// A file the store wrote (or found, or made of a clone and writes) whose data may still be
/// only in memory: its write-back is not known complete. It cannot be cloned: there is no
/// method for it. It becomes a [`Flushed`] with the completion of its [`Written::flush`].
#[derive(Debug)]
pub(crate) struct Written {
    name: FileName,
}

/// A file whose data is on its device as far as the store knows: it saw the file's write-back
/// complete and has written nothing to it since, or the file is one its commit synced. The
/// one thing a clone is made from.
#[derive(Clone, Debug)] // clone: a file's name, a reference count: each clone in flight holds its source's
pub(crate) struct Flushed {
    name: FileName,
}

impl Written {
    /// The file at `name`, which this store just wrote, derived or took over.
    pub(crate) fn wrote(name: FileName) -> Written {
        Written { name }
    }

    pub(crate) fn name(&self) -> &FileName {
        &self.name
    }

    /// Its write-back begun ([`Durable::Started`]): the effect whose completion,
    /// [`blaze_core::Done::WrittenBack`], makes it a [`Flushed`]. Nothing waits for it but
    /// what wants to clone the file.
    pub(crate) fn flush(&self, tag: Tag) -> Effect {
        // clone: the effect owns the name of the file it writes back
        Effect::WriteBack { file: self.name.clone(), tag }
    }

    /// The file once its [`Written::flush`] completed, given that completion: a [`Flushed`]
    /// for [`Done::WrittenBack`] and nothing written since, else the file back, still not to
    /// be cloned. The one way a file the store wrote becomes one it clones: a clone of a file
    /// whose write-back no one saw complete is a type error, not a review's finding.
    pub(crate) fn flushed(self, completion: &Done) -> Result<Flushed, Written> {
        match completion {
            Done::WrittenBack => Ok(Flushed { name: self.name }),
            _ => Err(self),
        }
    }

    /// The planted bug of `tests::place_crash`: the file taken as written back with no
    /// completion of its write-back seen.
    #[cfg(test)]
    pub(crate) fn planted_flushed(self) -> Flushed {
        Flushed { name: self.name }
    }
}

impl Flushed {
    /// A file the store's commit synced (a value kept as a file of its own, named by the
    /// head): durable, so clean.
    pub(crate) fn synced(name: FileName) -> Flushed {
        Flushed { name }
    }

    /// `to` made a clone of it, sharing its blocks: the only clone the store asks for, and
    /// of a file with no data left to write back, so the file system shares at once.
    pub(crate) fn clone_to(&self, to: FileName, tag: Tag) -> Effect {
        // clone: the effect owns the name of the file it clones
        Effect::Clone { from: self.name.clone(), to, tag }
    }
}

/// `bytes` as the whole of `file`, replaced in one step for a reader, taken as far toward the
/// disk as `durable` says. [`Durable::Started`] is the write of [`Durable::No`]: the caller
/// begins the write-back of the [`Written`] it then holds.
pub(crate) fn write(file: FileName, bytes: Vec<u8>, durable: Durable, tag: Tag) -> Effect {
    match durable {
        Durable::No | Durable::Started => Effect::WriteUnsynced { file, bytes, tag },
        // durable: asked for by the caller in its argument, which says why where it is given
        Durable::Yes => Effect::Write { file, bytes, tag },
    }
}

/// `from` renamed over `to` in one step for a reader, the new name taken as far toward the
/// disk as `durable` says.
pub(crate) fn rename(from: FileName, to: FileName, durable: Durable, tag: Tag) -> Effect {
    match durable {
        Durable::No | Durable::Started => Effect::RenameUnsynced { from, to, tag },
        // durable: asked for by the caller in its argument, which says why where it is given
        Durable::Yes => Effect::Rename { from, to, tag },
    }
}

/// A file as the runs of it that hold bytes: what lies between them, and from the last to
/// `len`, is zeros, which a file written from it keeps as holes (never written, no blocks).
/// What a linker hands over of an image whose patch room is most of its length: hello
/// world's dev executable is 162 MB of which 134 MB are two 64 MiB regions of zeros.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Sparse {
    /// The file's length.
    pub len: u64,
    /// Offsets and their bytes, by offset, none overlapping and none ending past `len`.
    pub runs: Vec<(u64, Vec<u8>)>,
}

impl Sparse {
    /// The bytes its runs hold: what a write of it writes.
    pub fn bytes(&self) -> u64 {
        self.runs.iter().map(|it| it.1.len() as u64).sum()
    }

    /// Why it is no file, naming the run: one that begins before the run before it ends, or
    /// ends past the length.
    pub fn check(&self) -> Result<(), String> {
        let mut end = 0u64;
        for (at, bytes) in &self.runs {
            if *at < end {
                // alloc: an error, once per place refused
                return Err(format!(
                    "the run at {at:#x} begins before {end:#x}, where the run before it ends"
                ));
            }
            end = at.saturating_add(bytes.len() as u64);
            if end > self.len {
                // alloc: an error, once per place refused
                return Err(format!(
                    "the run at {at:#x} ends at {end:#x}, past the file's length {:#x}",
                    self.len
                ));
            }
        }
        Ok(())
    }
}

/// An output being written from the runs its maker holds ([`Content::Sparse`]), by the tag
/// its place was asked with: its staged name cleared, cut to the file's length (a file of
/// holes), each run written at its offset, then the whole renamed onto its path, which until
/// then holds what it held. Nothing is synced and nothing is cloned: every step is one a
/// driver carries out without waiting for a device. That is the driver's to keep: a write at
/// an offset goes through the page cache on every driver (until 2026-11-11 io_uring's opened
/// a run of a MiB or more `O_DIRECT`, and an image's runs, cut at 8 MiB, each waited for
/// the disk: 1513 ms of an edit's answer, behind the profile's fsync). Each step is a span
/// of its own (`place step`, `function` saying which), so a place that is slow again names
/// the step it waited in.
///
/// A new file renamed over the path, never the path's file written in place: a program
/// running the path's last file holds that file (on Linux a write of it is refused,
/// `ETXTBSY`, and anywhere its mapped pages would change under it), and a reader of the
/// path sees the old bytes or the new, whole.
///
/// An output that is the path's own file with a few ranges written ([`Content::Over`]: an
/// edit's link, whose patch is a few kilobytes of a file of megabytes) starts from a clone
/// of that file in place of the file of holes: after the clear, the path's file is cloned
/// under the staged name, then cut, written and renamed as the other is. So what it writes
/// is the patch and never the file (until 2026-11-11 every edit wrote the image's runs
/// whole at each path, 21 MB twice for hello world: 27 ms of the answer). The clone is of
/// the path's own file and of no file of the store's; what it can force out is what that
/// file had written since its write-back, which every place asks for as it ends
/// ([`crate::Store::runs_step`]) and no place waits for: the last patch's ranges at most.
///
/// A clone shares its source's blocks and none of the pages the system holds of it in
/// memory, so the program's next start would read from the disk every page its last run had
/// in memory (measured 2026-11-11 on APFS, hello world's dev image: 30 to 45 ms of the first
/// `exec` after each edit, against 4 ms for the file before it). So as the clone is made the
/// system is asked to read into it what it holds of the path's file ([`Effect::Prewarm`],
/// [`Step::Warm`]), beside the cut and the writes. The rename is asked only once that
/// completed: which pages the path's file holds is read as the effect is carried out, and
/// after the rename the path names the clone. `tests::the_rename_waits_for_the_prewarm`
/// holds that order, with the rename asked first as its planted bug.
pub(crate) struct Writing {
    pub(crate) to: FileName,
    /// The hidden name beside `to` it is made under ([`crate::Store::staging_name`]).
    pub(crate) temp: FileName,
    pub(crate) len: u64,
    /// Whether it starts from a clone of the path's own file ([`Content::Over`]).
    pub(crate) over: bool,
    /// The runs, until the cut is made and they go to their writes.
    runs: Vec<(u64, Vec<u8>)>,
    /// The writes still in flight, and the first that failed.
    left: u32,
    failed: Option<String>,
    /// The cut is made and its runs are at their writes (or it has none).
    cut: bool,
    /// The prewarm of the clone is in flight ([`Step::Warm`]), with its span.
    warming: Option<tracing::Span>,
    /// The bytes its runs wrote.
    wrote: u64,
    /// The step in flight, open from its effects asked to the last of their completions
    /// (slow-is-p0-traced): never entered, as the place's own span is not.
    span: tracing::Span,
    /// The planted bug of `tests::the_rename_waits_for_the_prewarm`: the rename is asked as
    /// the writes end, whatever the prewarm.
    #[cfg(test)]
    pub(crate) plant_rename_before_warm: bool,
    /// The planted bug of `tests::place_made`, and how far it got.
    #[cfg(test)]
    plant: Option<Plant>,
    #[cfg(test)]
    phase: u8,
}

/// A safeguard of [`Writing`] taken out, for the controls of `tests::place_made`: the route
/// an edit's output took until 2026-11-10, a file of the store's written and then cloned to
/// the path.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    /// The file just written is cloned to the path at once: the clone forces its data out.
    ClonesUnflushed,
    /// The file just written is written back, waited for, and then cloned to the path: the
    /// answer waits for a write-back.
    WaitsForWriteBack,
}

/// Which effect of a [`Writing`] a completion is of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Step {
    /// The staged name removed.
    Clear,
    /// The path's own file cloned under the staged name ([`Content::Over`]).
    Clone,
    /// The staged name made a file of the length.
    Cut,
    /// One run written.
    Write,
    /// The clone's cache asked to hold what the path's file's does ([`Effect::Prewarm`]).
    Warm,
    /// The staged file renamed onto the path.
    Rename,
}

/// What a step of a [`Writing`] left of it ([`Writing::step`]).
pub(crate) enum Next {
    /// Effects asked; more completions to come.
    Going,
    /// The path holds the file: the bytes written to make it.
    Done(u64),
    /// It failed, with why; the staged file is the caller's to remove.
    Failed(String),
}

/// The span of one step of a [`Writing`] of `to`: `function` says which step, `bytes` what its
/// writes carry. No parent: it is opened under whatever completion asked its effects and
/// ends under another, as the place's own span does.
fn step_span(to: &FileName, step: &'static str, bytes: u64) -> tracing::Span {
    tracing::info_span!(target: "blaze", parent: None, "place step", function = step, %to, bytes)
}

impl Writing {
    /// The write of `file` to `to` under `temp` begun for the place `asked`: the effect that
    /// clears the staged name, whose completion is its first step ([`Step::Clear`]).
    pub(crate) fn begin(
        to: FileName,
        temp: FileName,
        file: Sparse,
        over: bool,
        asked: Tag,
    ) -> (Writing, Effect) {
        // clone: the effect and the state each own the name; once per place
        let clear = Effect::Remove { file: temp.clone(), tag: asked };
        let Sparse { len, runs } = file;
        let span = step_span(&to, "clear its staged name", 0);
        let writing = Writing {
            to,
            temp,
            len,
            over,
            runs,
            left: 0,
            failed: None,
            cut: false,
            warming: None,
            wrote: 0,
            span,
            #[cfg(test)]
            plant_rename_before_warm: false,
            #[cfg(test)]
            plant: None,
            #[cfg(test)]
            phase: 0,
        };
        (writing, clear)
    }

    /// How its `step` went (`done`), and what it asks next, each effect under a tag `ask`
    /// gives for the step it is. Bounded: one clear, one cut, a write a run, one rename, each
    /// an effect that completes or fails.
    pub(crate) fn step(
        &mut self,
        step: Step,
        done: &Done,
        mut ask: impl FnMut(Step) -> Tag,
        effects: &mut Vec<Effect>,
    ) -> Next {
        match (step, done) {
            (Step::Clear, Done::Removed) if self.over => {
                // The path's own file, its blocks shared: what the runs do not write is what
                // it holds.
                // clone: the effect owns the names; once per place
                let (from, to) = (self.to.clone(), self.temp.clone());
                self.span = step_span(&self.to, "clone the path's file", 0);
                effects.push(Effect::Clone { from, to, tag: ask(Step::Clone) });
                Next::Going
            }
            (Step::Clear, Done::Removed) | (Step::Clone, Done::Cloned) => {
                if step == Step::Clone {
                    // What the system holds in memory of the path's file, asked for in the
                    // clone: beside the cut and the writes, and before the rename.
                    // clone: the effect owns the names; once per place
                    let (like, file) = (self.to.clone(), self.temp.clone());
                    self.warming =
                        Some(step_span(&self.to, "read ahead what its last file held in memory", 0));
                    effects.push(Effect::Prewarm { like, file, tag: ask(Step::Warm) });
                }
                // The file of holes: made at its length, so what no run writes reads as zeros
                // and takes no block. A clone is cut to the length the same way.
                // clone: the effect owns the name; once per place
                let file = self.temp.clone();
                self.span = step_span(&self.to, "cut it to its length", 0);
                effects.push(Effect::Truncate { file, len: self.len, tag: ask(Step::Cut) });
                Next::Going
            }
            (Step::Warm, done) => {
                if let Some(span) = self.warming.take()
                    && let Done::Prewarmed { bytes } = done
                {
                    span.record("bytes", *bytes);
                }
                if !matches!(done, Done::Prewarmed { .. }) && self.failed.is_none() {
                    // alloc: an error, once per place that fails
                    self.failed = Some(format!(
                        "asking the system to read ahead what it holds of {}: {}",
                        self.to,
                        crate::describe(done)
                    ));
                }
                self.join(ask, effects)
            }
            // alloc: an error, once per place that fails
            (Step::Clone, done) => Next::Failed(format!(
                "cloning {} to {} to write what changed of it: {}",
                self.to,
                self.temp,
                crate::describe(done)
            )),
            (Step::Cut, Done::Truncated) => {
                self.cut = true;
                let runs = std::mem::take(&mut self.runs);
                if runs.is_empty() {
                    return self.join(ask, effects);
                }
                self.left = runs.len() as u32;
                let bytes = runs.iter().map(|it| it.1.len() as u64).sum();
                self.span = step_span(&self.to, "write its runs", bytes);
                for (at, bytes) in runs {
                    self.wrote += bytes.len() as u64;
                    // clone: each write's effect owns the name
                    let file = self.temp.clone();
                    effects.push(Effect::WriteAt { file, at, bytes, tag: ask(Step::Write) });
                }
                Next::Going
            }
            (Step::Write, done) => {
                self.left = self.left.saturating_sub(1);
                // (Under a planted bug its write-back and its clone come back as writes do.)
                let well = matches!(done, Done::Written)
                    || (cfg!(test) && matches!(done, Done::WrittenBack | Done::Cloned));
                if !well && self.failed.is_none() {
                    // alloc: an error, once per place that fails
                    self.failed =
                        Some(format!("writing its runs into {}: {}", self.temp, crate::describe(done)));
                }
                self.join(ask, effects)
            }
            (Step::Rename, Done::Renamed) => Next::Done(self.wrote),
            // alloc: an error, once per place that fails
            (Step::Clear, done) => Next::Failed(format!(
                "clearing {} to write it there: {}",
                self.temp,
                crate::describe(done)
            )),
            // alloc: an error, once per place that fails
            (Step::Cut, done) => Next::Failed(format!(
                "making {} a file of {} bytes: {}",
                self.temp,
                self.len,
                crate::describe(done)
            )),
            // alloc: an error, once per place that fails
            (Step::Rename, done) => Next::Failed(format!(
                "renaming {} onto its path: {}",
                self.temp,
                crate::describe(done)
            )),
        }
    }

    /// The rename, once the cut is made, every write has ended and the prewarm has (it reads
    /// what the path's file holds, which the rename would replace); or the first failure of
    /// them, once all have ended.
    fn join(&mut self, ask: impl FnMut(Step) -> Tag, effects: &mut Vec<Effect>) -> Next {
        #[cfg(test)]
        let warming = self.warming.is_some() && !self.plant_rename_before_warm;
        #[cfg(not(test))]
        let warming = self.warming.is_some();
        if !self.cut || self.left > 0 || warming {
            return Next::Going;
        }
        match self.failed.take() {
            None => self.commit(ask, effects),
            Some(why) => Next::Failed(why),
        }
    }

    /// The staged file, whole, renamed onto its path: one step for a reader. Not synced: the
    /// path holds its old file or the new one, and a build that finds neither places it again.
    fn commit(&mut self, mut ask: impl FnMut(Step) -> Tag, effects: &mut Vec<Effect>) -> Next {
        #[cfg(test)]
        if let Some(next) = self.planted(&mut ask, effects) {
            return next;
        }
        // clone: the effect owns the names; once per place
        let (from, to) = (self.temp.clone(), self.to.clone());
        self.span = step_span(&self.to, "rename it onto its path", 0);
        effects.push(rename(from, to, Durable::No, ask(Step::Rename)));
        Next::Going
    }
}

#[cfg(test)]
impl Writing {
    /// It with `plant` planted.
    pub(crate) fn planted_with(mut self, plant: Option<Plant>) -> Writing {
        self.plant = plant;
        self
    }

    /// The planted bugs ([`Plant`]): the file written is written back and waited for, or not,
    /// then cloned under another name, which is what is renamed onto the path. `None` once
    /// the plant has run its course (or with none planted): the rename follows.
    fn planted(&mut self, ask: &mut impl FnMut(Step) -> Tag, effects: &mut Vec<Effect>) -> Option<Next> {
        match (self.plant?, self.phase) {
            (Plant::WaitsForWriteBack, 0) => {
                self.phase = 1;
                self.left = 1;
                // clone: a test's planted effect owns the name
                effects.push(Effect::WriteBack { file: self.temp.clone(), tag: ask(Step::Write) });
                Some(Next::Going)
            }
            (Plant::WaitsForWriteBack, 1) | (Plant::ClonesUnflushed, 0) => {
                self.phase = 2;
                self.left = 1;
                let copy = FileName::new(&format!("{}.cloned", self.temp))
                    .expect("a name with a suffix is a name");
                // clone: a test's planted effect owns the name
                let from = std::mem::replace(&mut self.temp, copy.clone());
                effects.push(Effect::Clone { from, to: copy, tag: ask(Step::Write) });
                Some(Next::Going)
            }
            _ => None,
        }
    }
}

/// What a path is to hold ([`crate::Store::publish`]).
pub(crate) enum Content {
    /// These bytes: written where they go. `how`: `write`.
    Bytes(Vec<u8>),
    /// These runs, the rest of the file holes: written under a staged name and renamed onto
    /// the path ([`Writing`]). `how`: `write runs`.
    Sparse(Sparse),
    /// The path's own file with these runs written over it, cut to the length: a clone of
    /// it under a staged name, written and renamed onto the path ([`Writing`]). `how`:
    /// `patch`.
    Over(Sparse),
    /// `from`'s, `len` bytes that hash to `content`: cloned under a staged name and renamed
    /// onto the path. `of`, the whole file of the store's it is, counted as cloned until the
    /// clone ends. `how`: `clone`.
    File { from: Flushed, content: ContentId, of: Option<Source>, len: u64 },
    /// `base`'s (the whole file `base_id`, counted as cloned until the clone ends) with the
    /// ranges of the derived value `id` written over it and cut to `len`: cloned under a
    /// staged name, written into and renamed onto the path. `how`: `derive`.
    Patch { base: Flushed, base_id: ContentId, id: ContentId, len: u64 },
}

impl Content {
    /// What the place's span says of it.
    pub(crate) fn how(&self) -> &'static str {
        match self {
            Content::Bytes(_) => "write",
            Content::Sparse(_) => "write runs",
            Content::Over(_) => "patch",
            Content::File { .. } => "clone",
            Content::Patch { .. } => "derive",
        }
    }
}

/// An output being made of a clone of its base with a derived value's ranges written
/// ([`Content::Patch`]), by the tag its place was asked with.
pub(crate) struct Patching {
    pub(crate) to: FileName,
    /// The hidden name beside `to` it is made under ([`crate::Store::staging_name`]).
    pub(crate) temp: FileName,
    pub(crate) base_id: ContentId,
    pub(crate) id: ContentId,
    pub(crate) len: u64,
    /// The writes and the cut still in flight, and the first that failed.
    pub(crate) left: u32,
    pub(crate) failed: Option<String>,
    /// The bytes its ranges wrote.
    pub(crate) wrote: u64,
    /// Bytes written into the store that this place is the first to report.
    pub(crate) written: u64,
}

#[cfg(test)]
mod tests {
    use blaze_core::IoError;

    use super::*;

    /// What a [`Writing`] asked and was told, in order.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Event {
        Asked(Step),
        Told(Step),
    }

    /// The step an effect of a [`Writing`] is.
    fn step_of(effect: &Effect) -> Step {
        match effect {
            Effect::Remove { .. } => Step::Clear,
            Effect::Clone { .. } => Step::Clone,
            Effect::Truncate { .. } => Step::Cut,
            Effect::WriteAt { .. } => Step::Write,
            Effect::Prewarm { .. } => Step::Warm,
            Effect::RenameUnsynced { .. } => Step::Rename,
            other => panic!("no step of a writing: {other:?}"),
        }
    }

    /// A patch of two runs driven to its end, the completions of what is in flight at once
    /// given in the order `warm_first` says (the prewarm before the writes, or after), the
    /// prewarm answered as done or (`warmed` false) as failed: every event, and how it ended.
    fn driven(over: bool, warm_first: bool, warmed: bool, plant: bool) -> (Vec<Event>, Result<u64, String>) {
        let name = |it: &str| FileName::new(it).unwrap();
        let file = Sparse { len: 64, runs: vec![(0, vec![1; 8]), (32, vec![2; 8])] };
        let (mut writing, clear) = Writing::begin(name("out/a"), name("out/.a.placing"), file, over, Tag(1));
        writing.plant_rename_before_warm = plant;
        let mut events = vec![Event::Asked(step_of(&clear))];
        let mut flying = vec![Step::Clear];
        // Bounded: each turn completes one step in flight, and a writing asks a bounded
        // number of them.
        for _ in 0..32 {
            // The next completion: the prewarm first or last of what is in flight, as asked.
            let at = match flying.iter().position(|it| *it == Step::Warm) {
                Some(at) if warm_first || flying.len() == 1 => at,
                Some(at) => (at + 1) % flying.len(),
                None => 0,
            };
            let step = flying.remove(at);
            let done = match step {
                Step::Clear => Done::Removed,
                Step::Clone => Done::Cloned,
                Step::Cut => Done::Truncated,
                Step::Write => Done::Written,
                Step::Warm if warmed => Done::Prewarmed { bytes: 48 },
                Step::Warm => Done::Failed(IoError("no such file".to_owned())),
                Step::Rename => Done::Renamed,
            };
            events.push(Event::Told(step));
            let mut effects = Vec::new();
            match writing.step(step, &done, |_| Tag(2), &mut effects) {
                Next::Going => {}
                Next::Done(wrote) => return (events, Ok(wrote)),
                Next::Failed(why) => return (events, Err(why)),
            }
            for effect in &effects {
                events.push(Event::Asked(step_of(effect)));
                flying.push(step_of(effect));
            }
            assert!(!flying.is_empty(), "nothing in flight and no end: {events:?}");
        }
        panic!("a writing that does not end: {events:?}");
    }

    /// The oracle: why the events break the prewarm's order, if they do. The rename is asked
    /// before the prewarm was told (the prewarm would then read what the clone itself holds,
    /// the path naming it by then), or a prewarm is asked of a file that is no clone.
    fn out_of_order(events: &[Event], over: bool) -> Option<String> {
        let at = |event: Event| events.iter().position(|it| *it == event);
        let (asked, told) = (at(Event::Asked(Step::Warm)), at(Event::Told(Step::Warm)));
        let renamed = at(Event::Asked(Step::Rename));
        match (over, asked, told, renamed) {
            (false, Some(_), ..) => Some("a file of holes, which is no clone, was prewarmed".to_owned()),
            (true, None, ..) => Some("the clone was not prewarmed".to_owned()),
            (true, Some(_), None, Some(_)) => Some("the rename was asked and the prewarm never told".to_owned()),
            (true, Some(_), Some(told), Some(renamed)) if renamed < told => {
                Some(format!("the rename was asked (event {renamed}) before the prewarm was told (event {told})"))
            }
            _ => None,
        }
    }

    /// The invariant: a patch's rename onto its path is asked only once the prewarm of its
    /// clone was told, whichever of the prewarm and the writes ends first, since the prewarm
    /// reads what the system holds of the path's file, which the rename replaces; a file of
    /// holes asks for no prewarm; and a prewarm that failed fails the place, naming the path,
    /// once the writes ended, with no rename asked. The control: with the rename asked as the
    /// writes end (the planted bug), the oracle names the order.
    #[test]
    fn the_rename_waits_for_the_prewarm() {
        for warm_first in [true, false] {
            let (events, ended) = driven(true, warm_first, true, false);
            assert_eq!(ended, Ok(16), "{events:?}");
            assert_eq!(out_of_order(&events, true), None, "{events:?}");
            assert_eq!(events.last(), Some(&Event::Told(Step::Rename)));
        }
        let (events, ended) = driven(false, true, true, false);
        assert_eq!(ended, Ok(16), "{events:?}");
        assert_eq!(out_of_order(&events, false), None, "{events:?}");
        assert!(!events.contains(&Event::Asked(Step::Clone)), "{events:?}");

        for warm_first in [true, false] {
            let (events, ended) = driven(true, warm_first, false, false);
            let why = ended.expect_err("a prewarm that failed fails the place");
            assert!(why.contains("out/a") && why.contains("no such file"), "{why}");
            assert!(!events.contains(&Event::Asked(Step::Rename)), "{events:?}");
            let writes = events.iter().filter(|it| **it == Event::Told(Step::Write)).count();
            assert_eq!(writes, 2, "the place fails once its writes ended: {events:?}");
        }

        // The planted bug, with the prewarm told last: the rename is asked, and the place
        // ends, with the prewarm still in flight.
        let (events, _) = driven(true, false, true, true);
        let why = out_of_order(&events, true).expect("the oracle missed the rename asked first");
        assert!(why.contains("the rename was asked and the prewarm never told"), "{why}");
    }
}
