//! Other processes (docs/v2/plan/design/std.md, "The rest of the set", `Procs`): list them,
//! signal one, inspect one. Separate from one's own pid and exit, which are [`Host`]'s; made
//! from it ([`Host::procs`]) and narrowed to one process by [`Procs::only`], which never
//! widens.
//!
//! **Provider.** The simulation only ([`crate::hosted::SimHost::procs`]): blaze-core's one
//! signal effect is `Kill` of a child it spawned, and it has none that lists or signals
//! another process.
//!
//! [`Host`]: crate::host::Host
//! [`Host::procs`]: crate::host::Host::procs

use alloc::vec::Vec;
use core::fmt;

use blaze_mem::Frame;

use crate::{
    Stopped,
    cx::Cx,
    hosted::{self, Refused},
    io::Io,
    narrow::Allowed,
};

/// A process's id.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ProcId(pub u32);

impl fmt::Display for ProcId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Signal {
    Interrupt,
    Terminate,
    Kill,
    Stop,
    Continue,
}

/// What a process is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProcInfo {
    pub id: ProcId,
    pub parent: ProcId,
    pub alive: bool,
}

/// A process of the simulated host. A `Terminate` or a `Kill` sent to it ends it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SimProc {
    pub id: ProcId,
    pub parent: ProcId,
    pub alive: bool,
}

/// What a call on other processes was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProcsOp {
    List,
    Signal,
    Inspect,
}

/// A call on other processes that failed: the call, the process (none for a list) and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct ProcsError {
    pub op: ProcsOp,
    pub proc: Option<ProcId>,
    pub kind: Refused,
}

impl fmt::Display for ProcsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.proc {
            Some(proc) => write!(f, "bstd::procs {:?} of process {proc}: {}", self.op, self.kind),
            None => write!(f, "bstd::procs {:?} of the host's processes: {}", self.op, self.kind),
        }
    }
}

impl core::error::Error for ProcsError {}

/// Other processes: from [`crate::host::Host::procs`].
#[derive(Clone)]
pub struct Procs {
    io: Io,
    allowed: Allowed<ProcId>,
}

impl Procs {
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn new(io: Io) -> Procs {
        Procs { io, allowed: Allowed::Any }
    }

    /// Only the process `proc`, if this reaches it already.
    pub fn only(&self, proc: ProcId) -> Procs {
        Procs { io: self.io.clone(), allowed: self.allowed.one(&proc, self.io.plant()) }
    }

    /// Each process this reaches, in the host's order, in `frame`.
    pub fn list<'a>(&self, cx: &Cx, frame: &'a Frame) -> Result<Vec<ProcInfo, &'a Frame>, ProcsError> {
        let mut found = Vec::new_in(frame);
        let listed = hosted::call(&self.io, cx, |host| {
            for it in host.procs.iter().filter(|it| self.allowed.allows(&it.id)) {
                found
                    .try_reserve(1)
                    .map_err(|_| Stopped::OutOfMemory { arena: "bstd::procs list", asked: size_of::<ProcInfo>() })?;
                found.push(ProcInfo { id: it.id, parent: it.parent, alive: it.alive });
            }
            Ok(())
        });
        match listed {
            Ok(()) => Ok(found),
            Err(kind) => Err(ProcsError { op: ProcsOp::List, proc: None, kind }),
        }
    }

    /// `f` on the process `proc`, if this reaches it and the host has it.
    fn with_proc<R>(
        &self,
        cx: &Cx,
        op: ProcsOp,
        proc: ProcId,
        f: impl FnOnce(&mut SimProc) -> R,
    ) -> Result<R, ProcsError> {
        let refuse = |kind| ProcsError { op, proc: Some(proc), kind };
        if !self.allowed.allows(&proc) {
            return Err(refuse(Refused::NotAllowed));
        }
        hosted::call(&self.io, cx, |host| match host.procs.iter_mut().find(|it| it.id == proc) {
            Some(found) => Ok(f(found)),
            None => Err(Refused::NotFound),
        })
        .map_err(refuse)
    }

    /// What the process `proc` is.
    pub fn inspect(&self, cx: &Cx, proc: ProcId) -> Result<ProcInfo, ProcsError> {
        self.with_proc(cx, ProcsOp::Inspect, proc, |it| ProcInfo { id: it.id, parent: it.parent, alive: it.alive })
    }

    /// `signal` sent to the process `proc`.
    pub fn signal(&self, cx: &Cx, proc: ProcId, signal: Signal) -> Result<(), ProcsError> {
        self.with_proc(cx, ProcsOp::Signal, proc, |it| {
            if matches!(signal, Signal::Terminate | Signal::Kill) {
                it.alive = false;
            }
        })?;
        let _ = self.io.script(|host| host.signals.push((proc, signal)));
        Ok(())
    }
}

impl fmt::Debug for Procs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Procs({:?})", self.allowed)
    }
}
