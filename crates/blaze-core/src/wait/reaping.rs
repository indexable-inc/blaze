//! The reap of a child, a sans-IO core: reaped at once if it has exited, otherwise waited for
//! with the child left unreaped (`waitid(WEXITED | WNOWAIT)`, through the thread's wait ring
//! on Linux) and then reaped (`wait4(WNOHANG)`, which gives what it used). The wait never
//! reaps, so the one reap is the one that reports the child's status: a wait that reaped it
//! would leave the reap after it `ECHILD` and the exit unreported. Its proof is the DST in
//! `wait/tests.rs`, which plants that ([`ReapPlant::ReapInWait`]).

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::io;

use super::{Clock, Deadline, WaitError, Waited};

/// The kernel's children.
pub(crate) trait Children {
    /// What a reap says of a child: its status, and what it used.
    type Reaped;

    /// Reaps `pid` if it has exited, without blocking (`wait4(WNOHANG)`): `None` while it
    /// runs; an error (`ECHILD`) for a pid that is no child, or one reaped already.
    fn reap_now(&self, pid: i32) -> io::Result<Option<Self::Reaped>>;

    /// Waits until `pid` has exited, by `deadline` on `clock` (forever for `None`); with
    /// `keep`, leaves it unreaped (`WNOWAIT`).
    fn exited<C: Clock>(
        &self,
        what: &'static str,
        pid: i32,
        keep: bool,
        deadline: Option<Deadline>,
        clock: &C,
    ) -> Result<Waited<()>, WaitError>;
}

/// A safeguard of the reap the DST removes, to see that it catches the removal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ReapPlant {
    None,
    /// The wait reaps the child (no `WNOWAIT`): the reap after it finds no child, and the
    /// child's exit is never reported.
    #[cfg(test)]
    ReapInWait,
}

/// Reaps `pid` once it has exited, by `deadline` on `clock`: its [`Children::Reaped`], or
/// `Deadline` with the child still unreaped.
pub(crate) fn reap<K: Children, C: Clock>(
    what: &'static str,
    kernel: &K,
    pid: i32,
    deadline: Option<Deadline>,
    clock: &C,
    plant: ReapPlant,
) -> Result<Waited<K::Reaped>, WaitError> {
    if let Some(reaped) = kernel.reap_now(pid).map_err(|error| WaitError::io(what, error))? {
        return Ok(Waited::Ready(reaped));
    }
    let keep = plant == ReapPlant::None;
    match kernel.exited(what, pid, keep, deadline, clock)? {
        Waited::Deadline => return Ok(Waited::Deadline),
        Waited::Ready(()) => {}
    }
    match kernel.reap_now(pid).map_err(|error| WaitError::io(what, error))? {
        Some(reaped) => Ok(Waited::Ready(reaped)),
        None => Err(WaitError::io(
            what,
            io::Error::other(
                // alloc: an error path.
                format!(
                    "process {pid}: the wait said it exited, and the reap after found it running"
                ),
            ),
        )),
    }
}
