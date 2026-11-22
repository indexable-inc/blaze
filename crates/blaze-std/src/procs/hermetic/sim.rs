//! The simulation's hermetic child (beside `SimChild`): a scripted program ([`Act`]s) on a
//! virtual clock, run by a simulated OS ([`SimOs`]) that enforces the run's [`Plan`] exactly as
//! the OS backends render it, behind a [`Port`] ([`SimPort`]) and the caller's files
//! ([`SimFiles`]). The same [`super::run_through`] the backends call runs it, so the proof is of
//! the shipped path from spec to result.
//!
//! The OS model: a read is allowed when the plan allows it and the file is there; a write when
//! the plan allows it; everything else fails as the kernel would (the child sees an error and
//! goes on). A kill ends the child at once with `SIGKILL`; what it wrote stays on disk (the
//! machine must clear it). Each act takes [`STEP`] of virtual time; [`Act::Sleep`] and
//! [`Act::Burn`] take theirs, and `Burn` accrues CPU time as it goes.

use core::{cell::RefCell, time::Duration};

use blaze_own::{AVec, Alloc};
use blaze_roots::{Digest, RelPath, RootId, RootKind, Seen};

use super::{
    Access, Event, Fault, FilesError, FilesOp, HermeticSpec, Port, ReportParser, ReportSpec, RootFiles, StartView,
    Stream, Usage,
};
use crate::procs::Exit;

/// The virtual time one act takes.
pub const STEP: Duration = Duration::from_millis(1);

/// `SIGKILL`.
const SIGKILL: i32 = 9;

/// What a write writes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source<'s> {
    Bytes(&'s [u8]),
    /// Every byte the child has read so far.
    Seen,
    /// Its environment, `key=value` a line.
    Env,
}

/// One step of the script.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Act<'s> {
    /// A read the program reports (an `#include`): an absolute host spelling.
    Read { path: &'s str },
    /// A read the program does not report (the loader mapping a library).
    Load { path: &'s str },
    Write { path: &'s str, from: Source<'s> },
    Print { stream: Stream, bytes: &'s [u8] },
    Sleep { time: Duration },
    /// CPU time spent, and the memory held from its start.
    Burn { cpu: Duration, memory: u64 },
    Crash { signal: i32 },
    /// The end: the report written (when the spec asks for one), then the status.
    Exit { status: i32 },
}

/// The host's files: sorted by path.
pub struct SimOs<'a> {
    alloc: Alloc<'a>,
    files: AVec<'a, (AVec<'a, u8>, AVec<'a, u8>)>,
}

impl<'a> SimOs<'a> {
    pub fn new(alloc: Alloc<'a>) -> SimOs<'a> {
        SimOs { alloc, files: AVec::new_in(alloc) }
    }

    fn find(&self, path: &[u8]) -> Result<usize, usize> {
        self.files.binary_search_by(|(p, _)| p.as_slice().cmp(path))
    }

    /// `path` holds `bytes`.
    pub fn put(&mut self, path: &[u8], bytes: &[u8]) -> Result<(), Fault> {
        let mut content = AVec::new_in(self.alloc);
        content.try_extend_from_slice(bytes).map_err(|_| Fault::OutOfMemory)?;
        match self.find(path) {
            Ok(at) => self.files[at].1 = content,
            Err(at) => {
                let mut name = AVec::new_in(self.alloc);
                name.try_extend_from_slice(path).map_err(|_| Fault::OutOfMemory)?;
                self.files.try_reserve(1).map_err(|_| Fault::OutOfMemory)?;
                // Linear: a simulation's handful of files, kept sorted for `under`.
                self.files.insert_shift(at, (name, content));
            }
        }
        Ok(())
    }

    pub fn get(&self, path: &[u8]) -> Option<&[u8]> {
        self.find(path).ok().map(|at| self.files[at].1.as_slice())
    }

    /// Each file under the folder `folder` (not the folder's own spelling), in path order.
    pub fn under<'s>(&'s self, folder: &'s [u8]) -> impl Iterator<Item = (&'s [u8], &'s [u8])> + 's {
        self.files.iter().filter_map(move |(path, content)| {
            let rest = path.strip_prefix(folder)?.strip_prefix(b"/")?;
            Some((rest, content.as_slice()))
        })
    }

    /// Every file under `folder` removed.
    pub fn clear_under(&mut self, folder: &[u8]) {
        self.files.retain(|(path, _)| !path.strip_prefix(folder).is_some_and(|rest| rest.first() == Some(&b'/')));
    }

    /// Whether any file is there at all (the oracle's "nothing outside" check).
    pub fn has(&self, path: &[u8]) -> bool {
        self.find(path).is_ok()
    }
}

/// A report of one absolute path a line (the simulated program's format).
pub struct LinesReport;

impl ReportParser for LinesReport {
    fn paths<'t>(&self, text: &'t [u8], each: &mut dyn FnMut(&'t [u8]) -> Result<(), Fault>) -> Result<(), Fault> {
        for line in text.split(|&b| b == b'\n') {
            if !line.is_empty() {
                each(line)?;
            }
        }
        Ok(())
    }
}

/// The child's side: its script, run on the OS under the plan it was started with.
pub struct SimPort<'s, 'o, 'a> {
    os: &'o RefCell<SimOs<'a>>,
    alloc: Alloc<'a>,
    script: &'s [Act<'s>],
    at: usize,
    /// Of the act at `at`, the time left (a sleep or a burn cut by a wait's end).
    left: Option<Duration>,
    now: Duration,
    started: bool,
    exited: bool,
    cpu: Duration,
    memory: u64,
    pending: Option<Pending>,
    /// The plan as started: (text at, len, subtree, access).
    rule_text: AVec<'a, u8>,
    rules: AVec<'a, (u32, u32, bool, Access)>,
    /// The environment as given, `key=value\n` a pair: what the oracle compares.
    env: AVec<'a, u8>,
    /// Every byte read, and each reported read's spelling, `\n` after each.
    seen: AVec<'a, u8>,
    reported: AVec<'a, u8>,
    /// The report's absolute spelling, when the spec asks for one.
    report: Option<AVec<'a, u8>>,
    print: AVec<'a, u8>,
    /// What the start's confine step was asked: the plan's rule count, or none.
    confined: Option<u32>,
}

#[derive(Clone, Copy)]
enum Pending {
    Print { stream: Stream },
    Usage(Usage),
    Exited(Exit),
}

impl<'s, 'o, 'a> SimPort<'s, 'o, 'a> {
    /// The child of `script`, its clock starting at `now`.
    pub fn new(os: &'o RefCell<SimOs<'a>>, script: &'s [Act<'s>], now: Duration, alloc: Alloc<'a>) -> SimPort<'s, 'o, 'a> {
        SimPort {
            os,
            alloc,
            script,
            at: 0,
            left: None,
            now,
            started: false,
            exited: false,
            cpu: Duration::ZERO,
            memory: 0,
            pending: None,
            rule_text: AVec::new_in(alloc),
            rules: AVec::new_in(alloc),
            env: AVec::new_in(alloc),
            seen: AVec::new_in(alloc),
            reported: AVec::new_in(alloc),
            report: None,
            print: AVec::new_in(alloc),
            confined: None,
        }
    }

    /// The environment the child was given, `key=value\n` a pair.
    pub fn env_given(&self) -> &[u8] {
        &self.env
    }

    /// Each reported read that succeeded, `\n` after each.
    pub fn reported(&self) -> &[u8] {
        &self.reported
    }

    /// Whether the child was started at all.
    pub fn started(&self) -> bool {
        self.started
    }

    /// The confinement the start was asked with: the number of plan rules the child runs
    /// under, or `None` when it was started (or not started) without one.
    pub fn confined(&self) -> Option<u32> {
        self.confined
    }

    fn allows(&self, path: &[u8], access: Access) -> bool {
        if self.confined.is_none() {
            return true;
        }
        self.rules.iter().any(|&(at, len, subtree, rule)| {
            let granted = matches!((rule, access), (Access::Write, _) | (Access::Exec, Access::Exec | Access::Read) | (Access::Read, Access::Read));
            granted && super::plan::covers(&self.rule_text[at as usize..(at + len) as usize], subtree, path)
        })
    }

    fn read(&mut self, path: &str) -> Result<bool, Fault> {
        if !self.allows(path.as_bytes(), Access::Read) {
            return Ok(false);
        }
        let os = self.os.borrow();
        let Some(content) = os.get(path.as_bytes()) else { return Ok(false) };
        self.seen.try_extend_from_slice(content).map_err(|_| Fault::OutOfMemory)?;
        Ok(true)
    }

    fn write(&mut self, path: &str, from: Source<'_>) -> Result<(), Fault> {
        if !self.allows(path.as_bytes(), Access::Write) {
            return Ok(());
        }
        let bytes: &[u8] = match from {
            Source::Bytes(bytes) => bytes,
            Source::Seen => &self.seen,
            Source::Env => &self.env,
        };
        self.os.borrow_mut().put(path.as_bytes(), bytes)
    }

    fn end(&mut self, exit: Exit) {
        self.exited = true;
        self.pending = Some(Pending::Exited(exit));
    }

    /// The act at `at`, run (or begun, when it takes longer than is left before `until`).
    /// Returns whether an event is pending.
    fn act(&mut self, until: Duration) -> Result<bool, Fault> {
        let Some(&act) = self.script.get(self.at) else {
            self.end_with_report(0)?;
            return Ok(true);
        };
        match act {
            Act::Sleep { time } | Act::Burn { cpu: time, .. } => {
                if let Act::Burn { memory, .. } = act {
                    self.memory = self.memory.max(memory);
                }
                let left = self.left.unwrap_or(time);
                let room = until.saturating_sub(self.now);
                let spent = left.min(room);
                self.now += spent;
                if matches!(act, Act::Burn { .. }) {
                    self.cpu += spent;
                }
                if spent < left {
                    self.left = Some(left - spent);
                    return Ok(false);
                }
                self.left = None;
                self.at += 1;
                Ok(false)
            }
            other => {
                self.now += STEP;
                self.at += 1;
                match other {
                    Act::Read { path } => {
                        if self.read(path)? {
                            self.reported.try_extend_from_slice(path.as_bytes()).map_err(|_| Fault::OutOfMemory)?;
                            self.reported.try_push(b'\n').map_err(|_| Fault::OutOfMemory)?;
                        }
                        Ok(false)
                    }
                    Act::Load { path } => {
                        self.read(path)?;
                        Ok(false)
                    }
                    Act::Write { path, from } => {
                        self.write(path, from)?;
                        Ok(false)
                    }
                    Act::Print { stream, bytes } => {
                        self.print.clear();
                        self.print.try_extend_from_slice(bytes).map_err(|_| Fault::OutOfMemory)?;
                        self.pending = Some(Pending::Print { stream });
                        Ok(true)
                    }
                    Act::Crash { signal } => {
                        self.end(Exit::Signaled(signal));
                        Ok(true)
                    }
                    Act::Exit { status } => {
                        self.end_with_report(status)?;
                        Ok(true)
                    }
                    Act::Sleep { .. } | Act::Burn { .. } => Ok(false),
                }
            }
        }
    }

    fn end_with_report(&mut self, status: i32) -> Result<(), Fault> {
        if let Some(report) = &self.report {
            // The report is written under the output root: the plan allows it there.
            if self.allows(report, Access::Write) {
                self.os.borrow_mut().put(report, &self.reported)?;
            }
        }
        self.end(Exit::Exited(status));
        Ok(())
    }
}

impl Port for SimPort<'_, '_, '_> {
    fn now(&self) -> Duration {
        self.now
    }

    fn start(&mut self, start: &StartView<'_>) -> Result<(), Fault> {
        // The simulated start's confine step: the plan asked is what the child runs under, and
        // what was asked is recorded (`confined`). Without one (the planted bug) the child runs
        // unconfined, as a native start would if it let it through.
        match start.confine {
            Some(plan) => {
                for rule in plan.rules() {
                    let at = self.rule_text.len() as u32;
                    self.rule_text.try_extend_from_slice(rule.path.as_bytes()).map_err(|_| Fault::OutOfMemory)?;
                    self.rules.try_push((at, rule.path.len() as u32, rule.subtree, rule.access)).map_err(|_| Fault::OutOfMemory)?;
                }
                self.confined = Some(plan.len() as u32);
            }
            None => self.confined = None,
        }
        for (key, value) in start.env {
            for part in [key.as_bytes(), b"=", value.as_bytes(), b"\n"] {
                self.env.try_extend_from_slice(part).map_err(|_| Fault::OutOfMemory)?;
            }
        }
        if let ReportSpec::Program { file, .. } = start.spec.report() {
            let output = start.spec.roots().output().ok_or(Fault::NoOutputRoot)?;
            let mount = start.spec.mount_of(output).ok_or(Fault::Unmounted { root: output })?;
            let mut path = AVec::new_in(self.alloc);
            for part in [mount, b"/", file.as_str().as_bytes()] {
                path.try_extend_from_slice(part).map_err(|_| Fault::OutOfMemory)?;
            }
            self.report = Some(path);
        }
        self.started = true;
        Ok(())
    }

    fn wait(&mut self, until: Duration) -> Result<(), Fault> {
        while !self.exited && self.pending.is_none() && self.now < until {
            if self.act(until)? {
                return Ok(());
            }
        }
        Ok(())
    }

    fn meter(&mut self) -> Result<(), Fault> {
        self.pending = Some(Pending::Usage(Usage { cpu: self.cpu, memory: self.memory, steps: None }));
        Ok(())
    }

    fn kill(&mut self) -> Result<(), Fault> {
        if !self.exited {
            self.end(Exit::Signaled(SIGKILL));
        }
        Ok(())
    }

    fn next_event(&mut self) -> Option<Event<'_>> {
        match self.pending.take()? {
            Pending::Print { stream } => Some(Event::Bytes { stream, bytes: &self.print }),
            Pending::Usage(usage) => Some(Event::Usage(usage)),
            Pending::Exited(exit) => Some(Event::Exited(exit)),
        }
    }
}

/// The caller's files over the simulated OS: the roots as the spec mounts them.
pub struct SimFiles<'o, 'a, 's> {
    pub os: &'o RefCell<SimOs<'a>>,
    pub spec: &'s HermeticSpec<'s>,
    pub alloc: Alloc<'a>,
}

impl SimFiles<'_, '_, '_> {
    fn mount(&self, root: RootId, op: FilesOp) -> Result<&[u8], Fault> {
        self.spec.mount_of(root).ok_or(Fault::Files(FilesError { op, root, code: 0 }))
    }

    fn output(&self, op: FilesOp) -> Result<(RootId, &[u8]), Fault> {
        let root = self.spec.roots().output().ok_or(Fault::NoOutputRoot)?;
        Ok((root, self.mount(root, op)?))
    }
}

impl RootFiles for SimFiles<'_, '_, '_> {
    fn package_files(&self, root: RootId, each: &mut dyn FnMut(RelPath<'_>, Digest, u64) -> Result<(), Fault>) -> Result<(), Fault> {
        let mount = self.mount(root, FilesOp::PackageFiles)?;
        let declared = self.spec.roots().get(root).ok_or(Fault::Files(FilesError { op: FilesOp::PackageFiles, root, code: 0 }))?;
        if !matches!(declared.kind, RootKind::Package { .. }) {
            return Ok(());
        }
        let os = self.os.borrow();
        for (rest, content) in os.under(mount) {
            let Ok(path) = RelPath::from_bytes(rest, blaze_roots::Limits::SHIPPED.most_path) else { continue };
            if declared.admits_file(path) {
                each(path, Digest(crate::digest::blake3(content)), content.len() as u64)?;
            }
        }
        Ok(())
    }

    fn seen(&self, root: RootId, path: RelPath<'_>) -> Result<Seen, Fault> {
        let mount = self.mount(root, FilesOp::Seen)?;
        let mut full = AVec::new_in(self.alloc);
        for part in [mount, b"/", path.as_str().as_bytes()] {
            full.try_extend_from_slice(part).map_err(|_| Fault::OutOfMemory)?;
        }
        let os = self.os.borrow();
        Ok(match os.get(&full) {
            Some(content) => Seen::File { digest: Digest(crate::digest::blake3(content)), len: content.len() as u64 },
            None => Seen::Absent,
        })
    }

    fn read_output(&self, path: RelPath<'_>, most: u32, into: &mut AVec<'_, u8>) -> Result<(), Fault> {
        let (root, mount) = self.output(FilesOp::ReadOutput)?;
        let mut full = AVec::new_in(self.alloc);
        for part in [mount, b"/", path.as_str().as_bytes()] {
            full.try_extend_from_slice(part).map_err(|_| Fault::OutOfMemory)?;
        }
        let os = self.os.borrow();
        // A report the program did not write is an empty one: it read nothing it reports.
        let content = os.get(&full).unwrap_or(&[]);
        if content.len() > most as usize {
            return Err(Fault::Files(FilesError { op: FilesOp::ReadOutput, root, code: 0 }));
        }
        into.try_extend_from_slice(content).map_err(|_| Fault::OutOfMemory)
    }

    fn list_output(&self, each: &mut dyn FnMut(RelPath<'_>, Digest, u64) -> Result<(), Fault>) -> Result<(), Fault> {
        let (root, mount) = self.output(FilesOp::ListOutput)?;
        let os = self.os.borrow();
        for (rest, content) in os.under(mount) {
            let path = RelPath::from_bytes(rest, blaze_roots::Limits::SHIPPED.most_path)
                .map_err(|_| Fault::Files(FilesError { op: FilesOp::ListOutput, root, code: 0 }))?;
            each(path, Digest(crate::digest::blake3(content)), content.len() as u64)?;
        }
        Ok(())
    }

    fn clear_output(&self) -> Result<(), Fault> {
        let (_, mount) = self.output(FilesOp::ClearOutput)?;
        self.os.borrow_mut().clear_under(mount);
        Ok(())
    }
}
