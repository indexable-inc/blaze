//! The driver (D170 C5): thin, straight-line code over bstd's served child. It starts the
//! helper, writes what the machine sends, reads whole messages by the machine's deadline,
//! answers each lookup from the caller's [`View`] of the roots and reports the exit. It decides
//! nothing: every refusal is the machine's (job.rs).
//!
//! [`serve`] is written against bstd's `RunChild`, so the simulation's scripted child drives it
//! as the native [`bstd::procs::Served`] does (the conformance case, D167 c); [`run_native`]
//! starts the real helper.

use core::time::Duration;

use blaze_own::{AVec, Alloc, Vec, planted::Planted};
use bstd::{
    Deadline,
    cx::Cx,
    procs::{Exit, Input, Message, Output, ParentDeath, RunChild, RunError, RunSpec, Spawner, Streams, Waited},
};

use blaze_roots::{RelPath, RootId};

use crate::{
    job::{Effect, Ended, Event, Job, Phase},
    plant::JobPlant,
    wire::{Answer, LookOp, PREFIX},
};

/// The longest message the helper may send: its outputs come in pieces of at most 1 MiB.
pub const MOST_MESSAGE: u32 = 4 << 20;

/// What a lookup found in the view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Looked {
    Missing,
    /// A file, its digest and size; for a read its bytes are in the buffer given.
    File { digest: [u8; 32], size: u64 },
    Dir,
    /// A directory listed (for a [`LookOp::List`] alone): its names are in the buffer given,
    /// each followed by a NUL, sorted, a directory's with a `/` after it.
    Listing,
}

/// The roots as the caller holds them: the shelf's trees and the package's records, read in
/// place. The driver asks only for paths the machine resolved and admitted.
pub trait View {
    type Error;
    /// `path` under `root` looked up; for a [`LookOp::Read`] of a file its bytes, and for a
    /// [`LookOp::List`] its admitted names, added to `into`.
    fn look(&self, op: LookOp, root: RootId, path: RelPath<'_>, into: &mut AVec<'_, u8>) -> Result<Looked, Self::Error>;
}

/// Why the driver stopped (the job's own failures are the machine's, read by `Job::finish`).
#[derive(Debug)]
pub enum DriverError<E> {
    /// The child could not be started, written, read, waited for or killed.
    Run(RunError),
    View(E),
    /// The driver gave the machine an event it cannot take: a bug here.
    Step(crate::job::StepError),
    /// The machine asked for a second start, or for the child before its start.
    NoChild,
    /// The view's listing is not UTF-8 names (the view's bug).
    Listing,
}

/// The helper served until its job is done or failed.
pub fn serve<'a, C: RunChild, V: View, P: Planted<JobPlant>>(
    start: impl FnOnce() -> Result<C, RunError>,
    view: &V,
    job: &mut Job<'_, 'a, P>,
    alloc: Alloc<'a>,
) -> Result<(), DriverError<V::Error>> {
    let mut start = Some(start);
    let mut child: Option<C> = None;
    // bstd's channel reads into `alloc`'s `Vec`, not blaze-own's: the named gap in the D174
    // report ("what a base crate lacks").
    let mut inbox: alloc::vec::Vec<u8, Alloc<'a>> = alloc::vec::Vec::new_in(alloc);
    let mut file: AVec<'a, u8> = Vec::new_in(alloc);
    loop {
        while let Some(effect) = job.next_effect() {
            match effect {
                Effect::Spawn => {
                    let begin = start.take().ok_or(DriverError::NoChild)?;
                    let started = begin().map_err(DriverError::Run)?;
                    let now = nanos(started.now());
                    child = Some(started);
                    job.step(now, Event::Spawned).map_err(DriverError::Step)?;
                }
                Effect::Send(bytes) => {
                    let c = child.as_mut().ok_or(DriverError::NoChild)?;
                    c.send(bytes.as_slice(), deadline(job)).map_err(DriverError::Run)?;
                }
                Effect::Look { id, op, root, path } => {
                    let c = child.as_ref().ok_or(DriverError::NoChild)?;
                    file.clear();
                    // Borrows this lookup's `file`, so it lives for this answer alone.
                    let mut names: AVec<'a, &str> = Vec::new_in(alloc);
                    let looked = view.look(op, root, path.get(), &mut file).map_err(DriverError::View)?;
                    let answer = match (looked, op) {
                        (Looked::Missing, _) => Answer::Missing,
                        (Looked::Dir, _) => Answer::Dir,
                        (Looked::File { digest, size }, LookOp::Stat | LookOp::List) => {
                            Answer::File { digest, size, bytes: None }
                        }
                        (Looked::File { digest, size }, LookOp::Read) => {
                            Answer::File { digest, size, bytes: Some(file.as_slice()) }
                        }
                        (Looked::Listing, _) => {
                            let text = core::str::from_utf8(file.as_slice()).map_err(|_| DriverError::Listing)?;
                            let body = match text.strip_suffix('\0') {
                                Some(body) => body,
                                None if text.is_empty() => text,
                                None => return Err(DriverError::Listing),
                            };
                            if !text.is_empty() {
                                for name in body.split('\0') {
                                    names.push(name);
                                }
                            }
                            Answer::Listing { names: names.as_slice() }
                        }
                    };
                    // A listing for a stat, a file for a list: the machine refuses it by name
                    // (`JobError::AnswerMismatch`), the driver decides nothing.
                    job.step(nanos(c.now()), Event::Looked { id, answer }).map_err(DriverError::Step)?;
                }
                Effect::Kill => {
                    if let Some(c) = child.take() {
                        c.kill().map_err(DriverError::Run)?;
                    }
                }
            }
        }
        if matches!(job.state(), Phase::Done | Phase::Failed) {
            return Ok(());
        }
        let until = deadline(job);
        let c = child.take().ok_or(DriverError::NoChild)?;
        if job.state() == Phase::Ending {
            child = wait(c, until, job)?;
            continue;
        }
        let mut c = c;
        match c.recv_message(&mut inbox, MOST_MESSAGE, until).map_err(DriverError::Run)? {
            Message::Whole { len } => {
                job.step(nanos(c.now()), Event::Message(&inbox[PREFIX..len])).map_err(DriverError::Step)?;
                // `drain` wants `AllocatorNightly`, which blaze-own's `Alloc` is not: the rest
                // moved down, the end cut.
                let left = inbox.len() - len;
                inbox.copy_within(len.., 0);
                inbox.truncate(left);
                child = Some(c);
            }
            Message::Deadline => {
                job.step(nanos(c.now()), Event::Waited).map_err(DriverError::Step)?;
                child = Some(c);
            }
            Message::Closed => {
                job.step(nanos(c.now()), Event::Closed).map_err(DriverError::Step)?;
                child = wait(c, until, job)?;
            }
        }
    }
}

/// The child's end by `until`, told to the machine; the child back when it is still running.
pub(crate) fn wait<C: RunChild, E, P: Planted<JobPlant>>(
    c: C,
    until: Deadline,
    job: &mut Job<'_, '_, P>,
) -> Result<Option<C>, DriverError<E>> {
    // An end the wait sees came before `until`; it is told at the time the wait began.
    let before = nanos(c.now());
    match c.wait(until).map_err(DriverError::Run)? {
        Waited::Ended(reaped) => {
            let ended = match reaped.exit {
                Exit::Exited(code) => Ended::Exited(code),
                Exit::Signaled(signal) => Ended::Signaled(signal),
            };
            job.step(before, Event::Exited(ended)).map_err(DriverError::Step)?;
            Ok(None)
        }
        Waited::Running(c) => {
            job.step(nanos(c.now()), Event::Waited).map_err(DriverError::Step)?;
            Ok(Some(c))
        }
    }
}

/// The helper as a native child: no input, its outputs dropped (it writes only to its
/// channel), its own process group, killed at this process's death, its whole environment
/// empty. `helper` is its path on the shelf.
pub fn run_native<'a, V: View>(
    cx: &Cx,
    spawner: &Spawner,
    helper: &str,
    view: &V,
    job: &mut Job<'_, 'a, blaze_own::planted::NoPlant>,
    alloc: Alloc<'a>,
) -> Result<(), DriverError<V::Error>> {
    let spec = RunSpec {
        program: helper,
        arg0: None,
        args: &["--serve"],
        env: &[],
        dir: "/",
        streams: Streams { stdin: Input::Null, stdout: Output::Null, stderr: Output::Null },
        own_group: true,
        parent_death: ParentDeath::STRONGEST,
    };
    serve(|| spawner.start_served(cx, &spec).map(|started| started.child), view, job, alloc)
}

pub(crate) fn deadline<P: Planted<JobPlant>>(job: &Job<'_, '_, P>) -> Deadline {
    match job.next_deadline() {
        Some(at) => Deadline::at(Duration::from_nanos(at)),
        None => Deadline::PASSED,
    }
}

pub(crate) fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}
