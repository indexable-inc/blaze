//! The pinned clang as blaze-c's front end: [`ClangFrontEnd`] implements [`CFrontEnd`] over one
//! [`Job`] of the helper in declarations mode. It lives here, not in blaze-c, because blaze-c
//! depends on base crates alone (its Cargo.toml) and this crate depends on blaze-c.
//!
//! This module is a DRIVER: it holds the helper's process and does its blocking reads (each by
//! the job's deadline) when stepped with [`FrontInput::Tick`]. Everything it decides is the
//! job's machine or the import's: each lookup the helper makes becomes one ask of the import,
//! the place itself as a search of one folder, so the import's read list holds every probe,
//! present or absent, exactly as clang made it (clang does its own include search over its
//! virtual file system; the import does not search again).
//!
//! What the seam lacks, and how it is met here (each named in c.md "The toolchain side"):
//! - A directory: the seam asks for files alone. A stat of a root itself is answered as a
//!   directory here; any other stat is asked as a file, and a missing one answered
//!   [`Answer::NotAFile`] (no file; a directory to the compile), which leaves the outcome to the
//!   file asks beneath it. A listing is refused ([`TOOL_LIST`]).
//! - A typed fault for the tool: the job's own refusals reach blaze-c as
//!   `FrontFault::Diagnostic` with a code at or above [`TOOL`] ([`tool_code`]), until blaze-c
//!   has a variant for them.
//!
//! `now` is bstd's monotonic clock in nanoseconds, on every step and in the import's `Files`.

use core::time::Duration;

use blaze_c::{
    CAbi, CDialect, CFrontEnd, DeclsBuilder, DeclsOpts, Dir, FileNo, FrontEffect, FrontError, FrontFault, FrontInput,
    FrontState, PlaceError, Request, Root, SearchList, front::abi_row,
};
use blaze_own::{AString, AVec, Alloc, String, Vec, planted::NoPlant};
use blaze_roots::{Mounts, RelPath as RootPath, RootId, Roots};
use bstd::{
    Deadline,
    procs::{Message, RunChild, RunError},
};

use crate::{
    decls::{Census, DeclsLimits, DeclsReader, DeclsReaderOpts, Progress},
    driver::{MOST_MESSAGE, nanos, wait},
    flags::{self, Debug, Define, Lang, Opt, Source, Target, Toolchain, Unit},
    job::{Effect, Ended, Event, Job, JobError, JobOpts, Limits, Phase},
    wire::{Answer, Confinement, LookOp, Mode, PREFIX},
};

/// The first code of the tool's own faults, past any clang diagnostic id.
pub const TOOL: u32 = 0xC000_0000;
/// The request's target is not the one this front end was made for.
pub const TOOL_TARGET: u32 = TOOL | 1;
/// The request's headers, folders or definitions make no arguments (flags.rs `FlagFault`).
pub const TOOL_FLAGS: u32 = TOOL | 2;
/// The helper listed a directory: the seam has no listing.
pub const TOOL_LIST: u32 = TOOL | 3;
/// The declarations it sent are not `cfront-decls 2`; the code's low bits are the line.
pub const TOOL_DECLS: u32 = TOOL | 0x0100_0000;
/// It said the source has errors and sent no error row.
pub const TOOL_NO_DIAGNOSTIC: u32 = TOOL | 5;
/// The child could not be started, written, read or waited for.
pub const TOOL_RUN: u32 = TOOL | 6;
/// A lookup under a root of the run that is neither blaze-c root (`ClangOpts::places`).
pub const TOOL_PLACE: u32 = TOOL | 7;
/// A job refusal, by [`tool_code`]'s number for it.
pub const TOOL_JOB: u32 = TOOL | 0x100;

/// A job refusal's code: [`TOOL_JOB`] plus its variant's number.
pub fn tool_code(error: &JobError<'_>) -> u32 {
    let n = match error {
        JobError::Wire(_) => 0,
        JobError::Version { .. } => 1,
        JobError::Unconfined { .. } => 2,
        JobError::Unmounted { .. } => 3,
        JobError::Outside { .. } => 4,
        JobError::Reads(_) => 5,
        JobError::TooManyLooks { .. } => 6,
        JobError::FileOver { .. } => 7,
        JobError::OutputOver { .. } => 8,
        JobError::DiagnosticsOver { .. } => 9,
        JobError::UnknownLook { .. } => 10,
        JobError::AnswerMismatch { .. } => 11,
        JobError::OutOfStep { .. } => 12,
        JobError::Crashed { .. } => 13,
        JobError::NoOutput { .. } => 14,
        JobError::Deadline { .. } => 15,
        JobError::Job(_) => 16,
    };
    TOOL_JOB | n
}

/// What a [`ClangFrontEnd`] is made from: every choice required.
pub struct ClangOpts<'r, 'a, S> {
    /// Starts the helper (driver.rs `run_native`'s spawn, or the simulation's scripted child).
    pub start: S,
    pub roots: &'r Roots<'r>,
    pub mounts: &'r Mounts<'r>,
    /// Which blaze-c root each of the run's roots is: the package's, and the shelf's (the
    /// pinned clang and, under it, the target's headers: the SDK read by version or the pinned
    /// sysroot, both in the toolchain digest of the key: c.md decisions 1 and 3).
    pub places: &'r [(RootId, Root)],
    /// The guest spellings flags.rs puts on the command line.
    pub tools: Toolchain<'r>,
    pub target: Target,
    pub limits: Limits,
    pub decls: DeclsLimits,
    /// How long the whole parse may take, from its Begin, in nanoseconds.
    pub timeout: u64,
    pub confinement: Confinement,
    pub alloc: Alloc<'a>,
}

/// The ask out: the job's lookup it answers.
struct Asked<'a> {
    id: u32,
    op: LookOp,
    root: RootId,
    place: Root,
    path: AString<'a>,
}

/// The pinned clang, in its helper, as blaze-c's front end.
pub struct ClangFrontEnd<'r, 'a, C, S> {
    opts: ClangOpts<'r, 'a, Option<S>>,
    state: FrontState,
    job: Option<Job<'r, 'a, NoPlant>>,
    child: Option<C>,
    ask: Option<Asked<'a>>,
    /// One search each, of one folder: the root itself.
    package: SearchList<'a>,
    shelf: SearchList<'a>,
    /// The files answered so far, by root and path, for the error's [`FileNo`].
    files: AVec<'a, (RootId, AString<'a>, FileNo)>,
    inbox: alloc::vec::Vec<u8, Alloc<'a>>,
    abi: Option<CAbi>,
    done: Option<(DeclsBuilder<'a>, Census)>,
}

impl<'r, 'a, C: RunChild, S: FnOnce() -> Result<C, RunError>> ClangFrontEnd<'r, 'a, C, S> {
    pub fn new(opts: ClangOpts<'r, 'a, S>) -> Result<ClangFrontEnd<'r, 'a, C, S>, PlaceError> {
        let alloc = opts.alloc;
        let mut package = SearchList::new(alloc);
        package.push(Dir { root: Root::Package, path: None })?;
        let mut shelf = SearchList::new(alloc);
        shelf.push(Dir { root: Root::Shelf, path: None })?;
        let ClangOpts { start, roots, mounts, places, tools, target, limits, decls, timeout, confinement, alloc } = opts;
        Ok(ClangFrontEnd {
            opts: ClangOpts { start: Some(start), roots, mounts, places, tools, target, limits, decls, timeout, confinement, alloc },
            state: FrontState::Begin,
            job: None,
            child: None,
            ask: None,
            package,
            shelf,
            files: Vec::new_in(alloc),
            inbox: alloc::vec::Vec::new_in(alloc),
            abi: None,
            done: None,
        })
    }

    /// What the parse read, once [`FrontState::Done`]: the ladder's printed counts.
    pub fn census(&self) -> Option<&Census> {
        self.done.as_ref().map(|(_, census)| census)
    }

    fn fail(&mut self, file: Option<FileNo>, at: u32, fault: FrontFault) -> Result<(), FrontError> {
        self.state = FrontState::Failed;
        if let Some(c) = self.child.take() {
            // The job is over either way: its kill's own failure changes nothing it reports.
            let _ = c.kill();
        }
        Err(FrontError { file, at, fault })
    }

    fn begin(&mut self, now: u64, request: &Request<'_>, target: blaze_c::CTarget<'_>, dialect: CDialect) -> Result<(), FrontError> {
        if target.triple != self.opts.target.triple() {
            return self.fail(None, 0, FrontFault::Diagnostic { code: TOOL_TARGET });
        }
        let alloc = self.opts.alloc;
        let most = self.opts.limits.reads.most_path;
        let mut headers: AVec<'_, RootPath<'_>> = Vec::new_in(alloc);
        for h in request.headers.iter() {
            match RootPath::new(h.as_str(), most) {
                Ok(p) => headers.push(p),
                Err(_) => return self.fail(None, 0, FrontFault::Path(PlaceError::TooLong)),
            }
        }
        let mut include: AVec<'_, RootPath<'_>> = Vec::new_in(alloc);
        for d in request.include.iter() {
            match RootPath::new(d.as_str(), most) {
                Ok(p) => include.push(p),
                Err(_) => return self.fail(None, 0, FrontFault::Path(PlaceError::TooLong)),
            }
        }
        let mut values: AVec<'_, AString<'_>> = Vec::new_in(alloc);
        for d in request.defines.iter() {
            let mut text = String::new_in(alloc);
            if let Some(v) = d.value {
                if v.negative {
                    text.push('-');
                }
                text.push_str(v.text);
            }
            values.push(text);
        }
        let mut defines: AVec<'_, Define<'_>> = Vec::new_in(alloc);
        for (d, text) in request.defines.iter().zip(values.iter()) {
            defines.push(Define { name: d.name, value: d.value.map(|_| text.as_str()) });
        }
        let std = match dialect {
            CDialect::Gnu11 => "gnu11",
            CDialect::Gnu17 => "gnu17",
            CDialect::Gnu23 => "gnu23",
        };
        let unit = Unit {
            lang: Lang::C,
            std,
            target: self.opts.target,
            source: Source::Headers(headers.as_slice()),
            include: include.as_slice(),
            defines: defines.as_slice(),
            opt: Opt::Quick,
            debug: Debug::None,
        };
        let args = match flags::args(&unit, self.opts.tools, Mode::Decls, alloc) {
            Ok(args) => args,
            Err(_) => return self.fail(None, 0, FrontFault::Diagnostic { code: TOOL_FLAGS }),
        };
        self.abi = Some(target.abi);
        self.job = Some(Job::new(JobOpts {
            alloc,
            roots: self.opts.roots,
            mounts: self.opts.mounts,
            mode: Mode::Decls,
            args,
            limits: self.opts.limits,
            deadline: now.saturating_add(self.opts.timeout),
            confinement: self.opts.confinement,
            plant: NoPlant,
        }));
        self.state = FrontState::Working;
        self.pump(now)
    }

    /// The job's effects carried out until it asks for a file, ends or waits on the helper.
    fn pump(&mut self, now: u64) -> Result<(), FrontError> {
        loop {
            let Some(job) = self.job.as_mut() else {
                return self.fail(None, 0, FrontFault::OutOfState { state: self.state });
            };
            let Some(effect) = job.next_effect() else { break };
            match effect {
                Effect::Spawn => {
                    let Some(start) = self.opts.start.take() else {
                        return self.fail(None, 0, FrontFault::Diagnostic { code: TOOL_RUN });
                    };
                    match start() {
                        Ok(c) => {
                            let at = nanos(c.now());
                            self.child = Some(c);
                            self.step_job(at, Event::Spawned)?;
                        }
                        Err(_) => return self.fail(None, 0, FrontFault::Diagnostic { code: TOOL_RUN }),
                    }
                }
                Effect::Send(bytes) => {
                    let until = self.until();
                    let sent = self.child.as_mut().map(|c| c.send(bytes.as_slice(), until));
                    if !matches!(sent, Some(Ok(()))) {
                        return self.fail(None, 0, FrontFault::Diagnostic { code: TOOL_RUN });
                    }
                }
                Effect::Kill => {
                    if let Some(c) = self.child.take() {
                        // Killed because the job failed: the failure is the job's.
                        let _ = c.kill();
                    }
                }
                Effect::Look { id, op, root, path } => {
                    let rel = path.get();
                    if op == LookOp::List {
                        return self.fail(None, 0, FrontFault::Diagnostic { code: TOOL_LIST });
                    }
                    if rel.is_root() && op == LookOp::Stat {
                        self.step_job(now, Event::Looked { id, answer: Answer::Dir })?;
                        continue;
                    }
                    let Some(place) = self.opts.places.iter().find(|(r, _)| *r == root).map(|(_, p)| *p) else {
                        return self.fail(None, 0, FrontFault::Diagnostic { code: TOOL_PLACE });
                    };
                    if let Err(e) = blaze_c::RelPath::new(rel.as_str()) {
                        return self.fail(None, 0, FrontFault::Path(e));
                    }
                    let path = String::from_str_in(rel.as_str(), self.opts.alloc);
                    self.ask = Some(Asked { id, op, root, place, path });
                    self.state = FrontState::Reading;
                    return Ok(());
                }
            }
        }
        let phase = self.job.as_ref().map(|j| j.state());
        match phase {
            Some(Phase::Done) => self.finished(),
            Some(Phase::Failed) => {
                let error = self.job.as_ref().and_then(|j| j.error()).map(|e| match e {
                    JobError::Deadline { .. } => FrontFault::Deadline,
                    JobError::Crashed { ended: Ended::Exited(status), .. } => FrontFault::Ended { status: *status },
                    JobError::Crashed { ended: Ended::Signaled(signal), .. } => FrontFault::Ended { status: 128 + *signal },
                    other => FrontFault::Diagnostic { code: tool_code(other) },
                });
                self.fail(None, 0, error.unwrap_or(FrontFault::OutOfState { state: self.state }))
            }
            _ => {
                self.state = FrontState::Working;
                Ok(())
            }
        }
    }

    fn until(&self) -> Deadline {
        match self.job.as_ref().and_then(|j| j.next_deadline()) {
            Some(at) => Deadline::at(Duration::from_nanos(at)),
            None => Deadline::PASSED,
        }
    }

    fn step_job(&mut self, now: u64, event: Event<'_>) -> Result<(), FrontError> {
        let stepped = self.job.as_mut().map(|j| j.step(now, event));
        match stepped {
            Some(Ok(())) => Ok(()),
            _ => self.fail(None, 0, FrontFault::OutOfState { state: self.state }),
        }
    }

    /// One message, or the end, read from the helper by the job's deadline.
    fn receive(&mut self) -> Result<(), FrontError> {
        let until = self.until();
        let Some(mut c) = self.child.take() else {
            return self.fail(None, 0, FrontFault::OutOfState { state: self.state });
        };
        if self.job.as_ref().map(|j| j.state()) == Some(Phase::Ending) {
            let Some(job) = self.job.as_mut() else { return Ok(()) };
            return match wait::<C, (), NoPlant>(c, until, job) {
                Ok(back) => {
                    self.child = back;
                    Ok(())
                }
                Err(_) => self.fail(None, 0, FrontFault::Diagnostic { code: TOOL_RUN }),
            };
        }
        match c.recv_message(&mut self.inbox, MOST_MESSAGE, until) {
            Ok(Message::Whole { len }) => {
                let at = nanos(c.now());
                self.child = Some(c);
                let stepped = match self.job.as_mut() {
                    Some(job) => job.step(at, Event::Message(&self.inbox[PREFIX..len])),
                    None => return self.fail(None, 0, FrontFault::OutOfState { state: self.state }),
                };
                // As driver.rs: `drain` wants `AllocatorNightly`, which `Alloc` is not.
                let left = self.inbox.len() - len;
                self.inbox.copy_within(len.., 0);
                self.inbox.truncate(left);
                if stepped.is_err() {
                    return self.fail(None, 0, FrontFault::OutOfState { state: self.state });
                }
                Ok(())
            }
            Ok(Message::Deadline) => {
                let at = nanos(c.now());
                self.child = Some(c);
                self.step_job(at, Event::Waited)
            }
            Ok(Message::Closed) => {
                let at = nanos(c.now());
                self.step_job(at, Event::Closed)?;
                let Some(job) = self.job.as_mut() else { return Ok(()) };
                match wait::<C, (), NoPlant>(c, until, job) {
                    Ok(back) => {
                        self.child = back;
                        Ok(())
                    }
                    Err(_) => self.fail(None, 0, FrontFault::Diagnostic { code: TOOL_RUN }),
                }
            }
            Err(_) => self.fail(None, 0, FrontFault::Diagnostic { code: TOOL_RUN }),
        }
    }

    /// The job done: its first error as the front end's, or its declarations read into the
    /// model.
    fn finished(&mut self) -> Result<(), FrontError> {
        let Some(job) = self.job.take() else {
            return self.fail(None, 0, FrontFault::OutOfState { state: self.state });
        };
        let finished = match job.finish() {
            Ok(finished) => finished,
            Err(e) => return self.fail(None, 0, FrontFault::Diagnostic { code: tool_code(&e) }),
        };
        if finished.errors {
            let first = finished.diagnostics.first_error().map(|d| {
                let file = d.file.and_then(|(root, path)| {
                    self.files.iter().find(|(r, p, _)| *r == root && p.as_str() == path.as_str()).map(|(_, _, n)| *n)
                });
                (file, d.offset, d.code)
            });
            return match first {
                Some((file, at, code)) => self.fail(file, at, FrontFault::Diagnostic { code }),
                None => self.fail(None, 0, FrontFault::Diagnostic { code: TOOL_NO_DIAGNOSTIC }),
            };
        }
        let Some(abi) = self.abi else {
            return self.fail(None, 0, FrontFault::OutOfState { state: self.state });
        };
        let into = DeclsBuilder::new(DeclsOpts { abi: abi_row(&abi), alloc: self.opts.alloc });
        let mut reader = DeclsReader::new(DeclsReaderOpts {
            into,
            abi,
            mounts: self.opts.mounts,
            places: self.opts.places,
            limits: self.opts.decls,
            alloc: self.opts.alloc,
            plant: NoPlant,
        });
        let decls_fault = |line: u32, why: crate::decls::DeclsWhy| match why {
            crate::decls::DeclsWhy::Model(m) => FrontFault::Model(m),
            _ => FrontFault::Diagnostic { code: TOOL_DECLS | (line & 0x00FF_FFFF) },
        };
        match reader.feed(finished.decls.as_slice()) {
            Ok(Progress::Ended) => {}
            Ok(Progress::More) => return self.fail(None, 0, FrontFault::Diagnostic { code: TOOL_DECLS }),
            Err(e) => return self.fail(None, 0, decls_fault(e.line, e.why)),
        }
        match reader.finish() {
            Ok(done) => {
                self.done = Some(done);
                self.state = FrontState::Done;
                Ok(())
            }
            Err(e) => self.fail(None, 0, decls_fault(e.line, e.why)),
        }
    }
}

impl<'r, 'a, C: RunChild, S: FnOnce() -> Result<C, RunError>> CFrontEnd<'a> for ClangFrontEnd<'r, 'a, C, S> {
    fn step(&mut self, now: u64, input: FrontInput<'_>) -> Result<(), FrontError> {
        match (self.state, input) {
            (FrontState::Begin, FrontInput::Begin { request, target, dialect }) => self.begin(now, request, target, dialect),
            (FrontState::Reading, FrontInput::File { file, place: _, text }) => {
                let Some(Asked { id, op, root, path, .. }) = self.ask.take() else {
                    return self.fail(None, 0, FrontFault::OutOfState { state: self.state });
                };
                let digest = blaze_blake3::hash(text);
                let size = text.len() as u64;
                let bytes = match op {
                    LookOp::Read => Some(text),
                    LookOp::Stat | LookOp::List => None,
                };
                if !self.files.iter().any(|(r, p, _)| *r == root && p.as_str() == path.as_str()) {
                    self.files.push((root, path, file));
                }
                self.state = FrontState::Working;
                self.step_job(now, Event::Looked { id, answer: Answer::File { digest, size, bytes } })?;
                self.pump(now)
            }
            (FrontState::Reading, FrontInput::Missing) => {
                let Some(Asked { id, op, .. }) = self.ask.take() else {
                    return self.fail(None, 0, FrontFault::OutOfState { state: self.state });
                };
                // A stat that finds no file leaves the place to the file asks beneath it.
                let answer = match op {
                    LookOp::Stat => Answer::NotAFile,
                    LookOp::Read | LookOp::List => Answer::Missing,
                };
                self.state = FrontState::Working;
                self.step_job(now, Event::Looked { id, answer })?;
                self.pump(now)
            }
            (FrontState::Working, FrontInput::Tick) => {
                self.receive()?;
                self.pump(now)
            }
            (state, _) => self.fail(None, 0, FrontFault::OutOfState { state }),
        }
    }

    fn next_effect(&self) -> Option<FrontEffect<'_>> {
        let ask = self.ask.as_ref()?;
        let name = blaze_c::RelPath::new(ask.path.as_str()).ok()?;
        let search = match ask.place {
            Root::Package => self.package.as_search(),
            Root::Shelf => self.shelf.as_search(),
        };
        Some(FrontEffect::Read { name, search })
    }

    fn next_deadline(&self) -> Option<u64> {
        match self.state {
            FrontState::Working => self.job.as_ref().and_then(|j| j.next_deadline()),
            _ => None,
        }
    }

    fn state(&self) -> FrontState {
        self.state
    }

    fn finish(self) -> Result<DeclsBuilder<'a>, FrontError> {
        match (self.state, self.done) {
            (FrontState::Done, Some((decls, _))) => Ok(decls),
            (state, _) => Err(FrontError { file: None, at: 0, fault: FrontFault::OutOfState { state } }),
        }
    }
}
