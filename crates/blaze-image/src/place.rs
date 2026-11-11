//! Putting an image's bytes at its file, as a sans-IO core (rule sans-io-cores): [`Placer`] is
//! told the bytes [`crate::Builder`] made and hands back [`Effect`]s, one in flight at a time,
//! which the daemon's driver runs and answers; it owns no file and reads no clock.
//!
//! An image differs little from the one before it: a build changes the database's sections and
//! the plane's history, and leaves most of the VFS's tree and the library syntax trees as they
//! were. So the image is not written whole over the last. Where the file's bytes are known
//! ([`Placer::holds`]: the mapping of the image last placed, or the one the daemon started
//! from), the new image is made of them:
//!
//! 1. the name beside the file (`<file>.next`) is removed, in case a crash left one;
//! 2. the file is cloned to that name ([`Effect::Clone`]: its blocks shared, none copied);
//! 3. the clone is cut or grown to the new image's length;
//! 4. each run of [`UNIT`]-byte units that differ from the file's is written at its place,
//!    at most [`UNITS_A_WRITE`] units a write;
//! 5. the clone is synced;
//! 6. the clone is renamed over the file, which is the commit: a crash before it leaves the
//!    file the image it was, whole, and one after it the new image, whole.
//!
//! Units are compared byte for byte with the file's known bytes, never by a checksum, so the
//! file placed is the bytes [`crate::Builder`] made and no others (rule
//! persisted-state-round-trips): what a restore maps is equal to an image written whole.
//!
//! With no known bytes (a first image, or one whose file may have changed since it was
//! mapped), or where the file system refuses the clone (it cannot share blocks: ext4), the
//! image is written whole in one [`Effect::Write`], which every file system can do; the
//! refusal is told in [`Wrote::unshared`] for the daemon to say. That is the only other way a
//! new file can come to hold these bytes there, and what it leaves on disk is the same.
//!
//! A placing can be let go ([`Placer::let_go`]) while nothing of it is committed: the effect
//! in flight ends, the clone is removed, and the file is as it was. The daemon lets go of the
//! image it writes between requests when a request comes, so no request shares the disk with
//! an image's write.
//!
//! What the placer knows of the file's bytes it forgets when it asks for the rename or the
//! whole write, and learns again only from [`Placer::holds`]: a rename that failed after it
//! took place leaves bytes no one mapped, and they are never shared from.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{
    ops::Range,
    sync::atomic::{AtomicU32, Ordering::Relaxed},
};

use blaze_core::{Done, Effect, FileName, IoError, Tag};

use crate::Stable;

#[cfg(test)]
mod tests;

/// The bytes compared and written as one: 64 KiB, an image's default group.
pub const UNIT: usize = 1 << 16;

/// Units one job compares: 1 MiB, as one job of the CRCs.
const UNITS_A_JOB: usize = 16;

/// The most units one write holds (4 MiB): a placing let go ends within one such write.
pub const UNITS_A_WRITE: usize = 64;

/// How an image placed came to its file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Wrote {
    /// The image's length.
    pub len: u64,
    /// The bytes of it written; the rest are blocks shared with the image before it.
    pub written: u64,
    /// Why the image was written whole though the file's bytes were known: the clone was
    /// refused.
    pub unshared: Option<IoError>,
}

/// How a placing ended.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// The file holds the image, durably.
    Placed(Wrote),
    /// The image is not placed. The file is the image it was, unless the failure was the
    /// rename's or the whole write's, which may have taken place: its bytes are not known.
    Failed(IoError),
    /// The placing was let go before its commit: the file is the image it was.
    LetGo,
}

/// A safeguard left out, for the simulation's controls (`place/tests.rs`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Plant {
    #[default]
    None,
    /// The clone is renamed over the file without the sync before it.
    NoSync,
    /// The file's known bytes are kept across a rename, though no one mapped what it left.
    KeepsKnown,
}

enum Step {
    /// The whole image's write is in flight.
    Whole,
    /// The removal of a clone a crash left is in flight.
    Clearing,
    Cloning,
    Cutting,
    /// The write of run `next - 1` is in flight.
    Writing,
    Syncing,
    Renaming,
    /// The clone's removal is in flight; `why` is the failure that ended the placing, none
    /// for one let go.
    Dropping { why: Option<IoError> },
}

struct Job {
    /// The image; empty once a whole write took it.
    bytes: Vec<u8>,
    len: u64,
    /// The runs that differ from the file's bytes, in order, each at most
    /// [`UNITS_A_WRITE`] units.
    runs: Vec<Range<usize>>,
    next: usize,
    step: Step,
    written: u64,
    unshared: Option<IoError>,
    letting_go: bool,
}

/// The image's file and what is known of its bytes; one placing at a time.
pub struct Placer<S: Stable> {
    file: FileName,
    /// The name beside the file the next image is made at.
    temp: FileName,
    tag: Tag,
    /// The file's bytes, when they are known to be these.
    known: Option<S>,
    job: Option<Job>,
    pub plant: Plant,
}

impl<S: Stable> Placer<S> {
    /// The placer of the image at `file`, whose effects carry `tag`. `known` is the file's
    /// bytes as mapped at the start, none for a file not there or not read.
    pub fn new(file: FileName, tag: Tag, known: Option<S>) -> Placer<S> {
        // alloc: a name, once a daemon
        let temp = FileName::new(&format!("{file}.next"))
            .expect("an image's name with `.next` after it is a file's name");
        Placer { file, temp, tag, known, job: None, plant: Plant::None }
    }

    /// The file was mapped since the last placing ended and holds `bytes`: the next image
    /// shares what it can of them.
    pub fn holds(&mut self, bytes: S) {
        assert!(self.job.is_none(), "the image's file is mapped while an image is placed");
        self.known = Some(bytes);
    }

    /// Whether a placing is under way.
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Starts placing `bytes`, an image [`crate::Builder`] made; the units are compared on
    /// `jobs`. Its end is told by [`Placer::complete`].
    pub fn place(&mut self, bytes: Vec<u8>, jobs: crate::Jobs<'_>, effects: &mut Vec<Effect>) {
        assert!(self.job.is_none(), "an image placed while another is");
        let len = bytes.len() as u64;
        let mut job = Job {
            bytes,
            len,
            // alloc: empty, allocates nothing
            runs: Vec::new(),
            next: 0,
            step: Step::Whole,
            written: 0,
            unshared: None,
            letting_go: false,
        };
        let differing = self.known.as_ref().map(|known| differing(jobs, &job.bytes, known.bytes()));
        match differing {
            Some(runs) => {
                job.runs = runs;
                job.step = Step::Clearing;
                // clone: an effect owns its file's name; one an effect
                effects.push(Effect::Remove { file: self.temp.clone(), tag: self.tag });
            }
            None => self.whole(&mut job, effects),
        }
        self.job = Some(job);
    }

    /// Lets go of the placing under way, if nothing of it is committed: `true` when it will
    /// end as [`Outcome::LetGo`], `false` when there is none or its commit (the rename, or a
    /// whole write) is already asked, which then ends as it would have.
    pub fn let_go(&mut self) -> bool {
        match &mut self.job {
            Some(job) => match job.step {
                Step::Clearing | Step::Cloning | Step::Cutting | Step::Writing | Step::Syncing => {
                    job.letting_go = true;
                    true
                }
                Step::Dropping { .. } => job.letting_go,
                Step::Whole | Step::Renaming => false,
            },
            None => false,
        }
    }

    /// The whole image in one write: the file's bytes are not known from here on.
    fn whole(&mut self, job: &mut Job, effects: &mut Vec<Effect>) {
        self.known = None;
        job.written = job.len;
        job.step = Step::Whole;
        // clone: an effect owns its file's name; one an effect
        effects.push(Effect::Write { // durable: the image written whole is what a restart restores from: the old image or the new, never a part
            file: self.file.clone(),
            bytes: std::mem::take(&mut job.bytes),
            tag: self.tag,
        });
    }

    /// The clone removed, the placing ending with `why` once it is.
    fn drop_clone(&mut self, job: &mut Job, why: Option<IoError>, effects: &mut Vec<Effect>) {
        job.step = Step::Dropping { why };
        // clone: an effect owns its file's name; one an effect
        effects.push(Effect::Remove { file: self.temp.clone(), tag: self.tag });
    }

    /// The next run's write, or after the last the sync.
    fn write_next(&mut self, job: &mut Job, effects: &mut Vec<Effect>) {
        // clone: a range, two words
        match job.runs.get(job.next).cloned() {
            Some(run) => {
                job.next += 1;
                job.written += run.len() as u64;
                job.step = Step::Writing;
                effects.push(Effect::WriteAt {
                    // clone: an effect owns its file's name; one an effect
                    file: self.temp.clone(),
                    at: run.start as u64,
                    // alloc: an effect owns its bytes; at most `UNITS_A_WRITE` units a write
                    bytes: job.bytes[run].to_vec(),
                    tag: self.tag,
                });
            }
            None if self.plant == Plant::NoSync => self.rename(job, effects),
            None => {
                job.step = Step::Syncing;
                // clone: an effect owns its file's name; one an effect
                effects.push(Effect::Sync { file: self.temp.clone(), tag: self.tag }); // durable: the image's bytes are on disk before the rename names them (the DST's `Plant::NoSync` is this sync left out)
            }
        }
    }

    /// The commit: the file's bytes are not known from here on.
    fn rename(&mut self, job: &mut Job, effects: &mut Vec<Effect>) {
        if self.plant != Plant::KeepsKnown {
            self.known = None;
        }
        job.step = Step::Renaming;
        // clone: an effect owns its files' names; one an effect
        effects.push(Effect::Rename { from: self.temp.clone(), to: self.file.clone(), tag: self.tag }); // durable: the rename is the image's commit: a restart restores from the name it left
    }

    /// Takes the completion of its effect in flight (one of `tag`); says how the placing ended
    /// once it has.
    pub fn complete(&mut self, done: Done, effects: &mut Vec<Effect>) -> Option<Outcome> {
        let mut job = self.job.take().expect("a completion of the image's placing with none under way");
        let step = std::mem::replace(&mut job.step, Step::Whole);
        let placed = |job: &Job| {
            // clone: an error, once an image
            Outcome::Placed(Wrote { len: job.len, written: job.written, unshared: job.unshared.clone() })
        };
        let outcome = match (step, done) {
            (Step::Whole, Done::Written) | (Step::Renaming, Done::Renamed) => Some(placed(&job)),
            (Step::Whole, Done::Failed(error)) => Some(Outcome::Failed(error)),
            // The rename may have taken place and its directory's sync failed: the clone is
            // removed if it is still there, and the file's bytes stay unknown.
            (Step::Renaming, Done::Failed(error)) => {
                self.drop_clone(&mut job, Some(error), effects);
                None
            }
            (Step::Dropping { why: Some(error) }, Done::Removed | Done::Failed(_)) => {
                Some(Outcome::Failed(error))
            }
            (Step::Dropping { why: None }, Done::Removed | Done::Failed(_)) => Some(Outcome::LetGo),
            // Let go: whatever the effect in flight ended with, the clone goes.
            (
                Step::Clearing | Step::Cloning | Step::Cutting | Step::Writing | Step::Syncing,
                _,
            ) if job.letting_go => {
                self.drop_clone(&mut job, None, effects);
                None
            }
            (Step::Clearing, Done::Removed) => {
                job.step = Step::Cloning;
                effects.push(Effect::Clone {
                    // clone: an effect owns its files' names; one an effect
                    from: self.file.clone(),
                    to: self.temp.clone(),
                    tag: self.tag,
                });
                None
            }
            // No clone was made, so there is none to remove.
            (Step::Clearing, Done::Failed(error)) => Some(Outcome::Failed(error)),
            (Step::Cloning, Done::Cloned) => match job.len == self.known_len() {
                true => {
                    self.write_next(&mut job, effects);
                    None
                }
                false => {
                    job.step = Step::Cutting;
                    effects.push(Effect::Truncate {
                        // clone: an effect owns its file's name; one an effect
                        file: self.temp.clone(),
                        len: job.len,
                        tag: self.tag,
                    });
                    None
                }
            },
            // The file system shares no blocks, or the file is gone: the image whole.
            (Step::Cloning, Done::Failed(error)) => {
                job.unshared = Some(error);
                self.whole(&mut job, effects);
                None
            }
            (Step::Cutting, Done::Truncated) | (Step::Writing, Done::Written) => {
                self.write_next(&mut job, effects);
                None
            }
            (Step::Syncing, Done::Synced) => {
                self.rename(&mut job, effects);
                None
            }
            (Step::Cutting | Step::Writing | Step::Syncing, Done::Failed(error)) => {
                self.drop_clone(&mut job, Some(error), effects);
                None
            }
            (_, done) => unreachable!("an effect of the image's placing completed with {done:?}"),
        };
        if outcome.is_none() {
            self.job = Some(job);
        }
        outcome
    }

    /// The length of the file's known bytes; `u64::MAX` for none, which no image has.
    fn known_len(&self) -> u64 {
        self.known.as_ref().map_or(u64::MAX, |it| it.bytes().len() as u64)
    }
}

/// The byte ranges of `new` whose [`UNIT`]s differ from `old`'s at the same place (or lie past
/// its end), adjacent units joined, each range at most [`UNITS_A_WRITE`] units; the units are
/// compared on `jobs`.
fn differing(jobs: crate::Jobs<'_>, new: &[u8], old: &[u8]) -> Vec<Range<usize>> {
    let units = new.len().div_ceil(UNIT);
    let job_count = units.div_ceil(UNITS_A_JOB);
    // Bit `u` of job `j`'s word: unit `j * UNITS_A_JOB + u` differs.
    // alloc: one word a MiB of image, once an image
    let differs: Vec<AtomicU32> = (0..job_count).map(|_| AtomicU32::new(0)).collect();
    jobs(job_count, &|job| {
        let mut word = 0u32;
        for unit in job * UNITS_A_JOB..((job + 1) * UNITS_A_JOB).min(units) {
            let range = unit * UNIT..((unit + 1) * UNIT).min(new.len());
            if old.get(range.clone()) != Some(&new[range]) {
                word |= 1 << (unit % UNITS_A_JOB);
            }
        }
        differs[job].store(word, Relaxed);
    });
    // `jobs` returns after every job finished, which orders their stores before these loads.
    // alloc: the runs, a few an image
    let mut out: Vec<Range<usize>> = Vec::new();
    for unit in 0..units {
        if differs[unit / UNITS_A_JOB].load(Relaxed) & (1 << (unit % UNITS_A_JOB)) == 0 {
            continue;
        }
        let range = unit * UNIT..((unit + 1) * UNIT).min(new.len());
        match out.last_mut() {
            Some(last) if last.end == range.start && last.len() < UNITS_A_WRITE * UNIT => {
                last.end = range.end;
            }
            _ => out.push(range),
        }
    }
    out
}
