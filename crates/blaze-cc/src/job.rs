//! One run of the blaze-cfront helper, as a machine (D170 C5): it is spawned, says hello, is
//! given its job, asks for every file it opens and every directory it stats or lists, sends its
//! outputs and its diagnostics, and says it is done. Every lookup is resolved here, by the run's
//! [`Mounts`], to a root of its [`Roots`] and a [`RelPath`]: a path under no mount, or one its
//! root does not admit, ends the job with a refusal naming it before the driver looks; a lookup
//! the driver answers is pushed to the [`ReadList`], found, missing or listed, so the read list
//! is exact (c.md "The toolchain side", "Every file read"), and [`Job::finish`] hands it on
//! only as the proof [`Checked`].
//!
//! The driver (driver.rs) owns the child and the view of the roots; it turns [`Effect`]s into
//! calls and what comes back into [`Event`]s, and decides nothing. The import's front end
//! (front.rs) drives the same machine with blaze-c's import answering instead of a view.

#![blaze::sans_io]

use blaze_own::{ADeque, AString, AVec, Alloc, String, Vec, VecDeque, planted::Planted};
use blaze_roots::{
    Checked, Digest, Full, Mounts, Outside, ReadList, ReadListOpts, RelPath, RootId, Roots, Seen, Unmounted, listing_digest,
};

use crate::{
    flags::IMPORT_MAIN,
    plant::JobPlant,
    wire::{self, Answer, Confinement, FromHelper, JobFault, Level, LookOp, Mode, OutKind, WireFault},
};

/// What one job may use, each refused by name past its cap (rule 18).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// Lookups, found or not.
    pub most_looks: u32,
    /// Bytes of one file served.
    pub most_file: u64,
    /// Bytes of all outputs together.
    pub most_output: u64,
    /// Diagnostic rows, and the bytes of their messages together.
    pub most_diagnostics: u32,
    pub most_diagnostic_text: u32,
    /// The read list's caps, and the longest path a lookup may name.
    pub reads: blaze_roots::Limits,
}

impl Limits {
    /// For one translation unit: RocksDB's largest reads about 400 headers (an estimate from
    /// its include graph, not measured); a header past 64 MiB is not a header; past 4096
    /// diagnostics a unit is not worth reading further.
    pub const STANDARD: Limits = Limits {
        most_looks: 1 << 16,
        most_file: 64 << 20,
        most_output: 1 << 30,
        most_diagnostics: 4096,
        most_diagnostic_text: 4 << 20,
        reads: blaze_roots::Limits::SHIPPED,
    };
}

/// What [`Job::new`] takes: every choice required.
pub struct JobOpts<'r, 'a, P> {
    pub alloc: Alloc<'a>,
    pub roots: &'r Roots<'r>,
    /// The guest spellings of the roots, the ones flags.rs put on the command line.
    pub mounts: &'r Mounts<'r>,
    pub mode: Mode,
    /// The driver's arguments, the program first (flags.rs).
    pub args: AVec<'a, AString<'a>>,
    pub limits: Limits,
    /// When the job is refused, on the driver's clock, in nanoseconds.
    pub deadline: u64,
    /// The confinement this system offers, which the helper's hello must show (the driver
    /// knows the system; c.md "The operating system's hold").
    pub confinement: Confinement,
    /// `NoPlant` in shipped code.
    pub plant: P,
}

/// How the helper's process ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ended {
    Exited(i32),
    Signaled(i32),
}

/// Where a job is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Not yet spawned.
    Start,
    /// Spawned; its hello not yet come.
    Greeting,
    /// Given its job; asking and sending.
    Serving,
    /// It said it is done; its exit not yet seen.
    Ending,
    Done,
    Failed,
}

/// What the driver tells the machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event<'e> {
    /// The helper is running, its channel open.
    Spawned,
    /// One whole message's body, its prefix taken off.
    Message(&'e [u8]),
    /// Lookup `id` answered from the view of its root.
    Looked { id: u32, answer: Answer<'e> },
    /// The channel closed with nothing left in it.
    Closed,
    /// The helper's process ended.
    Exited(Ended),
    /// Time passed with nothing to tell: the deadline is looked at.
    Waited,
}

/// A looked-up path the machine owns: canonical, made only by [`Mounts::resolve`].
#[derive(Debug)]
pub struct LookPath<'a>(AString<'a>);

impl LookPath<'_> {
    pub fn get(&self) -> RelPath<'_> {
        // Written only from `Mounts::resolve`'s canonical output under the run's `most_path`,
        // so the check cannot fail; `ROOT` is unreachable, not a fallback.
        RelPath::new(self.0.as_str(), u32::MAX).unwrap_or(RelPath::ROOT)
    }
}

/// What the machine asks the driver for.
#[derive(Debug)]
pub enum Effect<'a> {
    /// Start the helper.
    Spawn,
    /// Write these whole messages to it.
    Send(AVec<'a, u8>),
    /// Look `path` up in the view of `root` and answer `id`. For a [`LookOp::List`] the
    /// answer's names are those the root admits (`Root::admits_file` or `admits_dir`), sorted.
    Look { id: u32, op: LookOp, root: RootId, path: LookPath<'a> },
    /// End it now.
    Kill,
}

/// Why a job failed: each names the thing (rule 19).
#[derive(Debug, PartialEq, Eq)]
pub enum JobError<'a> {
    /// A message from the helper that is not one.
    Wire(WireFault),
    /// The helper speaks another version.
    Version { got: u16 },
    /// The helper did not confine itself as this system allows.
    Unconfined { want: Confinement, got: Confinement },
    /// A guest path under no mount, or one that is not a path (`..` past `/`, too long).
    Unmounted { guest: AString<'a>, why: Unmounted },
    /// A path its root does not admit (outside the package's globs, or the output directory).
    Outside { root: RootId, path: AString<'a>, why: Outside },
    /// The read list is at its cap.
    Reads(Full),
    TooManyLooks { most: u32 },
    FileOver { root: RootId, path: AString<'a>, size: u64, most: u64 },
    OutputOver { most: u64 },
    DiagnosticsOver { most: u32 },
    /// An answer to a lookup the helper did not make.
    UnknownLook { id: u32 },
    /// An answer of the wrong kind for its lookup, a file whose bytes are not its size, or a
    /// listing unsorted or naming what its root does not admit (the driver's bug).
    AnswerMismatch { id: u32 },
    /// A message the helper may not send where it is.
    OutOfStep { phase: Phase },
    /// It ended before it said it was done, or not with status 0 (rule 20: one job fails).
    Crashed { ended: Ended, phase: Phase },
    /// It said it compiled and sent nothing.
    NoOutput { mode: Mode },
    Deadline { at: u64 },
    /// A message to it could not be written.
    Job(JobFault),
}

/// A driver's misuse of the machine: an event it cannot be given now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StepError {
    /// The job has ended; nothing more is taken.
    AfterEnd { phase: Phase },
    /// This event cannot come in this phase (a hello before the spawn, an exit before it).
    Unexpected { phase: Phase },
}

/// One diagnostic row, its file as a root and a path (none for the import's synthesized main
/// file, or a diagnostic about no file), its text in [`Diagnostics`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct DiagRow {
    level: Level,
    code: u32,
    offset: u32,
    root: Option<RootId>,
    path: (u32, u32),
    message: (u32, u32),
}

/// The helper's diagnostics, in the order sent.
pub struct Diagnostics<'a> {
    rows: AVec<'a, DiagRow>,
    text: AString<'a>,
}

/// One diagnostic as read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Diagnostic<'d> {
    pub level: Level,
    /// clang's diagnostic id (stable within one pinned clang, so it is part of no key).
    pub code: u32,
    /// The byte in its file.
    pub offset: u32,
    pub file: Option<(RootId, RelPath<'d>)>,
    pub message: &'d str,
}

impl<'a> Diagnostics<'a> {
    fn new(alloc: Alloc<'a>) -> Diagnostics<'a> {
        Diagnostics { rows: Vec::new_in(alloc), text: String::new_in(alloc) }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = Diagnostic<'_>> + '_ {
        self.rows.iter().map(move |row| {
            let slice = |(at, end): (u32, u32)| self.text.get(at as usize..end as usize).unwrap_or("");
            let file = row.root.map(|root| (root, RelPath::new(slice(row.path), u32::MAX).unwrap_or(RelPath::ROOT)));
            Diagnostic { level: row.level, code: row.code, offset: row.offset, file, message: slice(row.message) }
        })
    }

    /// The first error, if any.
    pub fn first_error(&self) -> Option<Diagnostic<'_>> {
        self.iter().find(|d| d.level >= Level::Error)
    }
}

/// What a finished job hands on.
pub struct Finished<'a> {
    /// Whether the unit's source has errors (then the diagnostics say which, and the object or
    /// declarations are absent).
    pub errors: bool,
    pub object: AVec<'a, u8>,
    pub decls: AVec<'a, u8>,
    pub diagnostics: Diagnostics<'a>,
    /// Every lookup, found, missing or listed, proven under the roots.
    pub reads: Checked<'a>,
}

enum State<'a> {
    Start,
    Greeting,
    Serving,
    Ending { errors: bool },
    Done { errors: bool },
    Failed(JobError<'a>),
}

struct Pending<'a> {
    id: u32,
    op: LookOp,
    root: RootId,
    path: AString<'a>,
}

/// One run of the helper.
pub struct Job<'r, 'a, P> {
    state: State<'a>,
    alloc: Alloc<'a>,
    roots: &'r Roots<'r>,
    mounts: &'r Mounts<'r>,
    mode: Mode,
    args: AVec<'a, AString<'a>>,
    limits: Limits,
    deadline: u64,
    confinement: Confinement,
    plant: P,
    effects: ADeque<'a, Effect<'a>>,
    pending: AVec<'a, Pending<'a>>,
    reads: ReadList<'a>,
    looks: u32,
    output: u64,
    object: AVec<'a, u8>,
    decls: AVec<'a, u8>,
    diagnostics: Diagnostics<'a>,
    /// Scratch for `Mounts::resolve` and a listing's child paths.
    scratch: AVec<'a, u8>,
}

impl<'r, 'a, P: Planted<JobPlant>> Job<'r, 'a, P> {
    pub fn new(opts: JobOpts<'r, 'a, P>) -> Job<'r, 'a, P> {
        let JobOpts { alloc, roots, mounts, mode, args, limits, deadline, confinement, plant } = opts;
        let mut effects = VecDeque::new_in(alloc);
        effects.push_back(Effect::Spawn);
        Job {
            state: State::Start,
            alloc,
            roots,
            mounts,
            mode,
            args,
            limits,
            deadline,
            confinement,
            plant,
            effects,
            pending: Vec::new_in(alloc),
            reads: ReadList::new(ReadListOpts { limits: limits.reads, alloc }),
            looks: 0,
            output: 0,
            object: Vec::new_in(alloc),
            decls: Vec::new_in(alloc),
            diagnostics: Diagnostics::new(alloc),
            scratch: Vec::new_in(alloc),
        }
    }

    pub fn state(&self) -> Phase {
        match self.state {
            State::Start => Phase::Start,
            State::Greeting => Phase::Greeting,
            State::Serving => Phase::Serving,
            State::Ending { .. } => Phase::Ending,
            State::Done { .. } => Phase::Done,
            State::Failed(_) => Phase::Failed,
        }
    }

    pub fn next_effect(&mut self) -> Option<Effect<'a>> {
        self.effects.pop_front()
    }

    /// The job's deadline, while it runs.
    pub fn next_deadline(&self) -> Option<u64> {
        match self.state {
            State::Done { .. } | State::Failed(_) => None,
            _ => Some(self.deadline),
        }
    }

    /// The error a failed job stopped with.
    pub fn error(&self) -> Option<&JobError<'a>> {
        match &self.state {
            State::Failed(error) => Some(error),
            _ => None,
        }
    }

    /// The diagnostics so far (the front end reads the first error of a failed parse).
    pub fn diagnostics(&self) -> &Diagnostics<'a> {
        &self.diagnostics
    }

    pub fn step(&mut self, now: u64, event: Event<'_>) -> Result<(), StepError> {
        let phase = self.state();
        if matches!(phase, Phase::Done | Phase::Failed) {
            return Err(StepError::AfterEnd { phase });
        }
        if now >= self.deadline {
            self.fail(JobError::Deadline { at: self.deadline });
            return Ok(());
        }
        match (phase, event) {
            (_, Event::Waited) => Ok(()),
            (Phase::Start, Event::Spawned) => {
                self.state = State::Greeting;
                Ok(())
            }
            (Phase::Start, _) | (_, Event::Spawned) => Err(StepError::Unexpected { phase }),
            (Phase::Greeting | Phase::Serving, Event::Message(body)) => {
                match wire::decode(body) {
                    Ok(message) => self.message(phase, message),
                    Err(fault) => self.fail(JobError::Wire(fault)),
                }
                Ok(())
            }
            (Phase::Ending, Event::Message(_)) => {
                self.fail(JobError::OutOfStep { phase });
                Ok(())
            }
            (Phase::Serving, Event::Looked { id, answer }) => {
                self.looked(id, answer);
                Ok(())
            }
            (_, Event::Looked { .. }) => Err(StepError::Unexpected { phase }),
            (_, Event::Closed) => Ok(()),
            (Phase::Ending, Event::Exited(Ended::Exited(0))) => {
                if let State::Ending { errors } = self.state {
                    self.state = State::Done { errors };
                }
                Ok(())
            }
            (_, Event::Exited(ended)) => {
                // It is gone: nothing to kill.
                self.state = State::Failed(JobError::Crashed { ended, phase });
                self.effects.clear();
                Ok(())
            }
            (Phase::Done | Phase::Failed, _) => Err(StepError::AfterEnd { phase }),
        }
    }

    /// The job's outputs and its checked read list, once it is done; its error once it failed.
    pub fn finish(self) -> Result<Finished<'a>, JobError<'a>> {
        let errors = match self.state {
            State::Done { errors } => errors,
            State::Failed(error) => return Err(error),
            _ => return Err(JobError::OutOfStep { phase: self.state() }),
        };
        let made = match self.mode {
            Mode::Compile => !self.object.is_empty(),
            Mode::Decls => !self.decls.is_empty(),
        };
        if !errors && !made {
            return Err(JobError::NoOutput { mode: self.mode });
        }
        let alloc = self.alloc;
        let reads = self.reads.under(self.roots).map_err(|refused| {
            let entry = refused.entry();
            JobError::Outside {
                root: entry.root,
                path: String::from_str_in(entry.path.as_str(), alloc),
                why: refused.why(),
            }
        })?;
        Ok(Finished { errors, object: self.object, decls: self.decls, diagnostics: self.diagnostics, reads })
    }

    fn fail(&mut self, error: JobError<'a>) {
        let spawned = !matches!(self.state, State::Start);
        self.state = State::Failed(error);
        self.effects.clear();
        if spawned {
            self.effects.push_back(Effect::Kill);
        }
    }

    fn message(&mut self, phase: Phase, message: FromHelper<'_>) {
        match (phase, message) {
            (Phase::Greeting, FromHelper::Hello { version, confinement }) => {
                if version != wire::VERSION {
                    return self.fail(JobError::Version { got: version });
                }
                if !covers(confinement, self.confinement) {
                    return self.fail(JobError::Unconfined { want: self.confinement, got: confinement });
                }
                let mut args: AVec<'_, &str> = Vec::new_in(self.alloc);
                for arg in self.args.iter() {
                    args.push(arg.as_str());
                }
                let mut out = Vec::new_in(self.alloc);
                match wire::job(&mut out, self.mode, args.as_slice()) {
                    Ok(()) => {
                        self.effects.push_back(Effect::Send(out));
                        self.state = State::Serving;
                    }
                    Err(fault) => self.fail(JobError::Job(fault)),
                }
            }
            (Phase::Serving, FromHelper::Look { id, op, path }) => self.look(id, op, path),
            (Phase::Serving, FromHelper::Out { kind, bytes }) => {
                self.output = self.output.saturating_add(bytes.len() as u64);
                if self.output > self.limits.most_output {
                    return self.fail(JobError::OutputOver { most: self.limits.most_output });
                }
                let into = match kind {
                    OutKind::Object => &mut self.object,
                    OutKind::Decls => &mut self.decls,
                };
                into.extend_from_slice(bytes);
            }
            (Phase::Serving, FromHelper::Diagnostic { level, code, offset, path, message }) => {
                self.diagnostic(level, code, offset, path, message);
            }
            (Phase::Serving, FromHelper::Done { errors }) => {
                if !self.pending.is_empty() {
                    return self.fail(JobError::OutOfStep { phase });
                }
                self.state = State::Ending { errors };
            }
            (phase, _) => self.fail(JobError::OutOfStep { phase }),
        }
    }

    fn diagnostic(&mut self, level: Level, code: u32, offset: u32, guest: &str, message: &str) {
        let most = self.limits.most_diagnostics;
        if u32::try_from(self.diagnostics.rows.len()).map_or(true, |n| n >= most) {
            return self.fail(JobError::DiagnosticsOver { most });
        }
        let text_most = self.limits.most_diagnostic_text;
        let root = match guest {
            // About no file, or about the import's main file, which is the arguments' own.
            "" => None,
            _ if guest == IMPORT_MAIN => None,
            _ => match self.resolve(guest) {
                Ok(root) => Some(root),
                Err(error) => return self.fail(error),
            },
        };
        let text = &mut self.diagnostics.text;
        let grows = self.scratch.len().saturating_add(message.len());
        if text.len().saturating_add(grows) > text_most as usize {
            return self.fail(JobError::DiagnosticsOver { most });
        }
        let start = text.len() as u32;
        if root.is_some() {
            // `resolve` left the path under its root in `scratch`, checked UTF-8.
            text.push_str(core::str::from_utf8(&self.scratch).unwrap_or(""));
        }
        let mid = text.len() as u32;
        text.push_str(message);
        let end = text.len() as u32;
        self.diagnostics.rows.push(DiagRow { level, code, offset, root, path: (start, mid), message: (mid, end) });
    }

    fn look(&mut self, id: u32, op: LookOp, guest: &str) {
        self.looks += 1;
        if self.looks > self.limits.most_looks {
            return self.fail(JobError::TooManyLooks { most: self.limits.most_looks });
        }
        if self.pending.iter().any(|p| p.id == id) {
            return self.fail(JobError::OutOfStep { phase: Phase::Serving });
        }
        let root = match self.resolve(guest) {
            Ok(root) => root,
            Err(error) => return self.fail(error),
        };
        let path = String::from_str_in(core::str::from_utf8(&self.scratch).unwrap_or(""), self.alloc);
        let rel = RelPath::new(path.as_str(), u32::MAX).unwrap_or(RelPath::ROOT);
        // Refused before the driver looks: a lookup no root admits as a file or a directory.
        let admitted = match (self.roots.get(root), op) {
            (None, _) => Err(Outside::UnknownRoot { root }),
            (Some(_), _) if Some(root) == self.roots.output() => Err(Outside::OutputRead { root }),
            (Some(r), LookOp::List) if !r.admits_dir(rel) => Err(Outside::NotDeclared { root }),
            (Some(r), _) if !r.admits_file(rel) && !r.admits_dir(rel) => Err(Outside::NotDeclared { root }),
            (Some(_), _) => Ok(()),
        };
        if let Err(why) = admitted {
            return self.fail(JobError::Outside { root, path, why });
        }
        let copy = String::from_str_in(path.as_str(), self.alloc);
        self.pending.push(Pending { id, op, root, path: copy });
        self.effects.push_back(Effect::Look { id, op, root, path: LookPath(path) });
    }

    /// `guest` as a root, its path under it left in `scratch`: `.` and empty segments dropped,
    /// `..` taking the segment before it away (blaze-roots' lexical rule: the trees hold no
    /// symbolic links), the longest mount winning.
    fn resolve(&mut self, guest: &str) -> Result<RootId, JobError<'a>> {
        let most = self.limits.reads.most_path;
        let resolved = if self.plant.has(JobPlant::DropsDotDot) {
            // The planted bug: `..` segments dropped before the lexical walk.
            let mut kept: AVec<'_, u8> = Vec::new_in(self.alloc);
            for (i, segment) in guest.split('/').filter(|s| *s != "..").enumerate() {
                if i > 0 {
                    kept.push(b'/');
                }
                kept.extend_from_slice(segment.as_bytes());
            }
            self.mounts.resolve(&kept, most, &mut self.scratch)
        } else {
            self.mounts.resolve(guest.as_bytes(), most, &mut self.scratch)
        };
        resolved.map_err(|why| JobError::Unmounted { guest: String::from_str_in(guest, self.alloc), why })
    }

    fn looked(&mut self, id: u32, answer: Answer<'_>) {
        let Some(at) = self.pending.iter().position(|p| p.id == id) else {
            return self.fail(JobError::UnknownLook { id });
        };
        let Pending { op, root, path, .. } = self.pending.swap_remove(at);
        let rel = RelPath::new(path.as_str(), u32::MAX).unwrap_or(RelPath::ROOT);
        let Some(r) = self.roots.get(root) else {
            return self.fail(JobError::Outside { root, path, why: Outside::UnknownRoot { root } });
        };
        let seen = match (op, answer) {
            (_, Answer::Missing) | (LookOp::Stat, Answer::NotAFile) => {
                if self.plant.has(JobPlant::ForgetsMisses) {
                    None
                } else {
                    Some(Seen::Absent)
                }
            }
            // clang's file manager opens a header candidate without a stat first: a directory
            // read is a directory seen, and the helper answers it `is a directory`.
            (LookOp::Stat | LookOp::Read, Answer::Dir) => Some(Seen::Dir),
            (LookOp::Stat, Answer::File { digest, size, bytes: None })
            | (LookOp::Read, Answer::File { digest, size, bytes: Some(_) }) => {
                if let Answer::File { bytes: Some(bytes), .. } = answer {
                    if bytes.len() as u64 != size {
                        return self.fail(JobError::AnswerMismatch { id });
                    }
                }
                if size > self.limits.most_file {
                    return self.fail(JobError::FileOver { root, path, size, most: self.limits.most_file });
                }
                if !r.admits_file(rel) {
                    return self.fail(JobError::Outside { root, path, why: Outside::NotDeclared { root } });
                }
                Some(Seen::File { digest: Digest(digest), len: size })
            }
            (LookOp::List, Answer::Listing { names }) => {
                // The digest is of what was sent, checked sorted and each name admitted here:
                // the driver's view decides nothing.
                for name in names {
                    let (child, dir) = match name.strip_suffix('/') {
                        Some(child) => (child, true),
                        None => (*name, false),
                    };
                    self.scratch.clear();
                    self.scratch.extend_from_slice(rel.as_str().as_bytes());
                    if !rel.is_root() {
                        self.scratch.push(b'/');
                    }
                    self.scratch.extend_from_slice(child.as_bytes());
                    // A name is one segment: a `/` inside it, `.` or `..` is refused.
                    let one = !child.contains('/') && !matches!(child, "" | "." | "..");
                    let parsed = core::str::from_utf8(&self.scratch).ok().and_then(|t| RelPath::new(t, u32::MAX).ok());
                    let ok = match parsed.filter(|_| one) {
                        Some(p) if dir => r.admits_dir(p),
                        Some(p) => r.admits_file(p),
                        None => false,
                    };
                    if !ok {
                        return self.fail(JobError::AnswerMismatch { id });
                    }
                }
                match listing_digest(names.iter().copied()) {
                    Ok(digest) => Some(Seen::Listing { digest }),
                    Err(_) => return self.fail(JobError::AnswerMismatch { id }),
                }
            }
            _ => return self.fail(JobError::AnswerMismatch { id }),
        };
        if let Some(seen) = seen {
            if let Err(full) = self.reads.push(root, rel, seen) {
                return self.fail(JobError::Reads(full));
            }
        }
        let mut out = Vec::new_in(self.alloc);
        match wire::answer(&mut out, id, &answer) {
            Ok(()) => self.effects.push_back(Effect::Send(out)),
            Err(fault) => self.fail(JobError::Job(fault)),
        }
    }
}

/// Whether what the helper did covers what this system offers.
fn covers(got: Confinement, want: Confinement) -> bool {
    got == want || (want == Confinement::Seccomp && got == Confinement::SeccompLandlock)
}
