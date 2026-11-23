//! File calls made on the asker's own thread (docs/v2/plan/design/std.md, "End state:
//! efficiency", pieces 7 and 11): a native driver whose file calls are blocking calls on its
//! threads ([`crate::kqueue::Kqueue`]) hands out an [`Inline`], and its asker makes the call
//! itself, with no queue, no lock and no thread hop, through the same function the driver's
//! threads run ([`super::run`]), its error made a [`Done::Failed`] the same way (`work_on` in
//! kqueue.rs). So an inline call's `Done` is the queued call's by construction, and
//! `inline_and_queued_calls_end_equal` holds the two equal over random scripts, with a planted
//! inline call that swallows an error to show the comparison catches one.
//!
//! The two routes are two instantiations of one seam (rule strategy-is-a-type-parameter): the
//! simulation never hands out an [`Inline`] (`Driver::inline_files` is `None` there), so every
//! effect it is asked keeps its queue, its shuffled completions, its failures and its crashes.
//!
//! Which effects go inline on a native driver that gives an [`Inline`], and why:
//!
//! | Effect | Route | Why, and what bounds it |
//! |---|---|---|
//! | `Stat`, `Lstat` | inline | one `stat`: the kernel answers from the inode cache or one device read |
//! | `Stamp` | inline | one `getattrlistbulk` pass or one `stat` a name, as above |
//! | `Entries` | inline | one directory's listing, one level: bounded by the directory's size |
//! | `ReadLink` | inline | one `readlink`; opens nothing |
//! | `Read`, `ReadAll` | inline | `open`, `pread`, `close` of a regular file: the page cache or the device, never another process (*) |
//! | `Append`, `WriteAt`, and [`Inline::write_lent`] | inline | into the page cache: the effects' own docs say they wait for no device (*) |
//! | `ReadInto`, `ReadMany` | queued | the store's pool: its depth is the threads' queue, and an idle read is throttled on a thread of the driver's, not the asker's |
//! | `Sync`, `SyncDir`, `Barrier`, `WriteBack` | queued | they wait for the device: a deadline must be able to end the asker's wait |
//! | `Clone`, `CopyData`, `Prewarm`, `List`, `Map`, `MapWrite` | queued | time in proportion to a file's or a tree's size: the deadline must be able to fire |
//! | `Write`, `WriteUnsynced`, `Truncate`, `Rename*`, `Remove*`, `MakeDir`, `Mode`, `Seal`, `Private`, `Times`, `Link`, `Ancestors`, `Above`, `WriteFrom` | queued | not measured yet: a later piece moves each with its numbers |
//! | timers, locks, watches, journals, processes, sockets, datagrams, the host's | queued | they wait for an event: only the queue can be given up at a deadline |
//!
//! (*) The one way these wait unbounded is a name that is not a regular file: `open` of a FIFO
//! waits for its writer. The driver's root holds what blaze and the build wrote and the
//! sources it reads; a FIFO there is a finding, and the queued route would have hung one of
//! the driver's threads on it as surely (verify: a guard by `O_NONBLOCK` at the open).
//!
//! Every inline call opens its span through [`crate::lock::io_wait`], naming what it does and
//! the file (rule every-wait-says-what), as the queued route's own thread does for its slow
//! calls. The asker's deadline is looked at before the call (bstd's `Io::call`), never during
//! it: hence the table.

use core::alloc::Allocator;
use std::{io, path::Path, sync::Arc};

use blaze_own::Share;

use crate::{Done, Effect, FileName, IoError, descriptors::Descriptors, fs::beneath::RootDir};

/// What a driver lets its asker do on the asker's own thread: the file calls of the table in
/// the module's doc, with the driver's root, its cached root `dirfd`, and its kept descriptors.
/// Cheap to clone (`Arc` counts); `Send + Sync`, held beside the asker's lock, never under it.
#[derive(Clone)]
pub struct Inline {
    root: Arc<Path>,
    root_dir: Arc<RootDir>,
    kept: Arc<Descriptors>,
    /// The planted bug of `inline_and_queued_calls_end_equal`: a failed inline call answers
    /// as an empty read, swallowing the error the queued route reports.
    #[cfg(test)]
    swallow: bool,
}

/// What [`Inline::perform_now`] did with an effect.
#[derive(Debug)]
pub enum Now {
    /// Made inline, ended: the `Done` the queue would have delivered.
    Done(Done),
    /// Not one for the asker's thread (the module's table): handed back whole, for the queue.
    Queue(Effect),
}

/// Where a lent write ([`Inline::write_lent`]) puts its bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lent {
    /// At the file's end, making it: an [`Effect::Append`]'s place.
    End,
    /// At this offset of the file, which exists, past its end making it longer: an
    /// [`Effect::WriteAt`]'s.
    At(u64),
}

/// What a lent whole-file read ([`Inline::read_all_lent`]) found.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an outcome with an error's text
pub enum LentRead {
    /// The file's `len` bytes, in the first `len` bytes of the room (`0` for a missing file, as
    /// the queued `ReadAll` answers it); the room's last byte is unused.
    Read { len: u64 },
    /// The asker gave no room for a file of `len` bytes (its allocator refused).
    NoRoom { len: u64 },
    /// The file's length moved between its `fstat` (`stated`) and its read (`read` bytes, one
    /// past `stated` when it grew). Not read again: a file written while it is read has no one
    /// "whole" to return, and a second read races the same writer (rule no-fallback: no retry);
    /// the asker learns it by name and decides.
    Moved { stated: u64, read: u64 },
    /// The call failed, saying this (the text a queued call's `Done::Failed` carries).
    Failed(IoError),
}

/// The span name an inline effect's call opens, or `None` for one that keeps its queue (the
/// module's table, the one place it is decided).
pub fn inline_op(effect: &Effect) -> Option<&'static str> {
    match effect {
        Effect::Stat { .. } => Some("stat"),
        Effect::Lstat { .. } => Some("lstat"),
        Effect::Stamp { .. } => Some("stamp"),
        Effect::Entries { .. } => Some("entries"),
        Effect::ReadLink { .. } => Some("readlink"),
        Effect::Read { .. } => Some("read"),
        Effect::ReadAll { .. } => Some("read all"),
        Effect::Append { .. } => Some("append"),
        Effect::WriteAt { .. } => Some("write at"),
        _ => None,
    }
}

/// What an inline call's span names when its effect names no one file: a column of names
/// ([`Effect::Stamp`]) or the root's listing.
const NO_ONE_FILE: &str = "a column of names, or the root";

/// The file an inline effect names, for its span; `None` for [`NO_ONE_FILE`].
fn named(effect: &Effect) -> Option<FileName> {
    // clone: the span's name, a refcount, kept while the effect moves into the call.
    match effect {
        Effect::Stat { file, .. }
        | Effect::Lstat { file, .. }
        | Effect::ReadLink { file, .. }
        | Effect::Read { file, .. }
        | Effect::ReadAll { file, .. }
        | Effect::Append { file, .. }
        | Effect::WriteAt { file, .. } => Some(file.share()),
        Effect::Entries { dir, .. } => dir.name().cloned(),
        _ => None,
    }
}

impl Inline {
    /// The inline calls of a driver over `root` whose threads read through `kept`.
    pub(crate) fn new(root: Arc<Path>, kept: Arc<Descriptors>) -> Inline {
        let root_dir = Arc::new(RootDir::open(&root));
        Inline {
            root,
            root_dir,
            kept,
            #[cfg(test)]
            swallow: false,
        }
    }

    /// `effect` made now on this thread when the module's table says it may be, its `Done`
    /// the one the driver's threads would have delivered; otherwise handed back for the queue.
    /// Asked before the effect is submitted; an effect made here never reaches the driver, so
    /// its `in_flight` does not count it.
    pub fn perform_now(&self, effect: Effect) -> Now {
        let Some(op) = inline_op(&effect) else { return Now::Queue(effect) };
        // The span's file, taken before the effect moves into the call.
        let file = named(&effect);
        let done = crate::lock::io_wait(op, file.as_ref().map_or(NO_ONE_FILE, FileName::as_str), || {
            super::run(&self.root, &self.kept, effect)
        });
        Now::Done(self.ended(done))
    }

    /// `bytes` written to `file` at `place` from the asker's memory, with no copy: the bytes
    /// are the kernel's only for the length of this call, which returns once the system holds
    /// them (the page cache), so the borrow outlives every use of them. Ends as the queued
    /// [`Effect::Append`] (`Done::Appended`) or [`Effect::WriteAt`] (`Done::Written`) of the
    /// same bytes would: the same system call through the same path join, the same error text.
    pub fn write_lent(&self, file: &FileName, place: Lent, bytes: &[u8]) -> Done {
        let done = match place {
            Lent::End => crate::lock::io_wait("append", file.as_str(), || {
                super::at(&self.root, file, |path| {
                    crate::fs::append(path, bytes)?;
                    Ok(Done::Appended)
                })
            }),
            Lent::At(offset) => crate::lock::io_wait("write at", file.as_str(), || {
                super::at(&self.root, file, |path| {
                    crate::fs::write_at(path, offset, bytes)?;
                    Ok(Done::Written)
                })
            }),
        };
        self.ended(done)
    }

    /// [`Effect::ReadAll`] of `file` straight into the asker's memory: `room(len)` is asked
    /// once, after the `fstat`, for exactly `len + 1` bytes (the last one says the file grew),
    /// and the file is read into it with no copy after the kernel's (blaze-core fs.rs
    /// `read_whole_into`). The calls: `open`, `fstat`, one `pread`, `close`, where the
    /// queued route's `std::fs::read` makes `open`, `fstat`, two `read`s and `close` and the
    /// asker then copies its `Vec`. Ends as the queued `ReadAll` would in what it reads; a
    /// length that moved is [`LentRead::Moved`].
    pub fn read_all_lent<'b>(&self, file: &FileName, room: impl FnOnce(u64) -> Option<&'b mut [u8]>) -> LentRead {
        let done = crate::lock::io_wait("read all", file.as_str(), || {
            super::at(&self.root, file, |path| crate::fs::read_whole_into(path, room))
        });
        match done {
            Ok(read) => read,
            #[cfg(test)]
            Err(_) if self.swallow => LentRead::Read { len: 0 },
            // alloc: an error path.
            Err(error) => LentRead::Failed(IoError(error.to_string())),
        }
    }

    /// Zero-memset [`Effect::ReadAll`] straight into `into`'s uninitialized spare capacity:
    /// `open`, `fstat`, `try_reserve(len + 1)` (NO zero-fill), `pread` into `spare_capacity_mut`,
    /// `set_len(old + len)`.
    pub fn read_all_into_vec<A: Allocator>(&self, file: &FileName, into: &mut Vec<u8, A>) -> LentRead {
        let done = crate::lock::io_wait("read all", file.as_str(), || {
            super::at(&self.root, file, |path| crate::fs::beneath::read_whole_into_vec(path, into))
        });
        match done {
            Ok(read) => read,
            #[cfg(test)]
            Err(_) if self.swallow => LentRead::Read { len: 0 },
            // alloc: an error path.
            Err(error) => LentRead::Failed(IoError(error.to_string())),
        }
    }

    /// Kernel-confined, zero-memset whole-file read: opens `file` strictly beneath this driver's
    /// cached root `dirfd` via `openat`/`O_NOFOLLOW` (or Linux `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`),
    /// rejecting any symbolic link in any directory component or final leaf with `ELOOP`, and
    /// reads straight into `into`'s uninitialized spare capacity.
    pub fn read_all_confined_into_vec<A: Allocator>(&self, file: &FileName, into: &mut Vec<u8, A>) -> LentRead {
        let done = crate::lock::io_wait("read all confined", file.as_str(), || {
            crate::fs::beneath::read_whole_confined_into_vec(&self.root_dir, &self.root, file, into)
        });
        match done {
            Ok(read) => read,
            #[cfg(test)]
            Err(_) if self.swallow => LentRead::Read { len: 0 },
            // alloc: an error path.
            Err(error) => LentRead::Failed(IoError(error.to_string())),
        }
    }

    /// A call's result as the driver's thread hands it back (`work_on`, kqueue.rs): its error
    /// a [`Done::Failed`] of the error's text.
    fn ended(&self, done: io::Result<Done>) -> Done {
        match done {
            Ok(done) => done,
            #[cfg(test)]
            Err(_) if self.swallow => Done::Read(Vec::new()),
            // alloc: an error path.
            Err(error) => Done::Failed(IoError(error.to_string())),
        }
    }

    /// The planted bug: every failed call answered as an empty read.
    #[cfg(test)]
    pub(crate) fn swallowing(self) -> Inline {
        Inline { swallow: true, ..self }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    //! `inline_and_queued_calls_end_equal`: a random script of the inline effects, made on
    //! one driver through its queue and on another, over a twin directory, inline (lent writes
    //! among them), ends in equal `Done`s, step by step. Its control plants an inline call that
    //! swallows an error, and the comparison catches it. Prints
    //! `inline and queued: 64 scripts, equal Dones; the swallowed error caught`.

    use std::time::{Duration, Instant};

    use blaze_sim::{Broke, Config, Tape};

    use super::{Inline, Lent, Now};
    use crate::{Completion, Done, Driver, Effect, FileName, Tag, kqueue::Kqueue, wait::Deadline};

    /// The script's names: two files, one beneath a folder, and one never made.
    const NAMES: [&str; 4] = ["a", "b", "d/c", "missing"];

    fn name(it: &str) -> FileName {
        FileName::new(it).expect("a test's name is plain")
    }

    /// One step: an effect of the table's inline set, and for a write, whether the inline side
    /// makes it as a lent write.
    fn draw(tape: &Tape, n: u64) -> (Effect, bool) {
        let file = name(NAMES[tape.index(NAMES.len(), "file")]);
        let tag = Tag(n);
        let lent = tape.coin("lent");
        let effect = match tape.below(7, "kind") {
            0 => Effect::Stat { file, tag },
            1 => Effect::Lstat { file, tag },
            2 => Effect::ReadAll { file, tag },
            3 => Effect::Read { file, at: tape.below(8, "at"), len: tape.below(8, "len") as usize, tag },
            4 => Effect::Append { file, bytes: vec![b'x'; 1 + tape.below(5, "n") as usize], tag },
            5 => Effect::WriteAt { file, at: tape.below(10, "at"), bytes: vec![b'y'; 1 + tape.below(4, "n") as usize], tag },
            _ => Effect::ReadLink { file, tag },
        };
        (effect, lent)
    }

    /// What a step's end says, without what two directories differ in (times, inodes).
    fn view(done: &Done) -> String {
        match done {
            Done::Stat(Some(meta)) => format!("stat {} {}", meta.len, meta.dir),
            // Its text may hold the directory's path, which the twins differ in.
            Done::Failed(_) => "failed".to_owned(),
            other => format!("{other:?}"),
        }
    }

    /// `effect` through `driver`'s queue, waited for (bounded: 10 s).
    fn queued(driver: &mut Kqueue, effect: Effect) -> Result<Done, Broke> {
        let tag = effect.tag();
        driver.submit(&mut vec![effect]);
        let until = Instant::now() + Duration::from_secs(10);
        let mut out: Vec<Completion> = Vec::new();
        loop {
            if let Some(at) = out.iter().position(|it| it.tag == tag) {
                return Ok(out.swap_remove(at).done);
            }
            if Instant::now() > until {
                return Err(Broke::Harness(format!("the queued {tag:?} did not end within 10 s")));
            }
            driver.poll(&mut out, Deadline::after(driver.now(), Duration::from_millis(100)));
        }
    }

    /// `effect` inline on `inline`, as a lent write when `lent` says so and it is a write.
    fn inline(inline: &Inline, effect: Effect, lent: bool) -> Result<Done, Broke> {
        match effect {
            Effect::Append { file, bytes, .. } if lent => Ok(inline.write_lent(&file, Lent::End, &bytes)),
            Effect::WriteAt { file, at, bytes, .. } if lent => Ok(inline.write_lent(&file, Lent::At(at), &bytes)),
            effect => match inline.perform_now(effect) {
                Now::Done(done) => Ok(done),
                Now::Queue(effect) => Err(Broke::Harness(format!("{effect:?} is in the table's inline set"))),
            },
        }
    }

    /// One script of up to 24 steps on twin directories; the first step whose ends differ.
    fn script(tape: &Tape, swallow: bool) -> Result<(), Broke> {
        let (ours, theirs) = (
            crate::temp::TempDir::for_test("blaze-inline-queued-"),
            crate::temp::TempDir::for_test("blaze-inline-inline-"),
        );
        let mut queue = Kqueue::new(&*ours, 2).map_err(|error| Broke::Harness(format!("a kqueue: {error}")))?;
        let made = Kqueue::new(&*theirs, 2).map_err(|error| Broke::Harness(format!("a kqueue: {error}")))?;
        let mut on = made.inline_files().ok_or_else(|| Broke::Harness("a kqueue gives its inline calls".into()))?;
        if swallow {
            on = on.swallowing();
        }
        let steps = 1 + tape.below(24, "steps");
        for n in 0..steps {
            let (effect, lent) = draw(tape, n);
            let asked = format!("{effect:?}");
            let (one, two) = (queued(&mut queue, effect.clone())?, inline(&on, effect, lent)?);
            if view(&one) != view(&two) {
                return Err(Broke::Oracle(format!(
                    "step {n}, {asked} (lent {lent}): queued ended {}, inline {}",
                    view(&one),
                    view(&two)
                )));
            }
        }
        Ok(())
    }

    #[test]
    fn inline_and_queued_calls_end_equal() {
        blaze_sim::check(Config::cases(64), |tape| script(tape, false));
        blaze_sim::control(Config::cases(64), false, true, "queued ended failed", script);
        println!("inline and queued: 64 scripts, equal Dones; the swallowed error caught");
    }
}
