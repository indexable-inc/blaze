//! The reap of a child, a sans-IO core: reaped at once if it has exited, otherwise waited for
//! with the child left unreaped (`waitid(WEXITED | WNOWAIT)`, through the thread's wait ring
//! on Linux) and then reaped (`wait4(WNOHANG)`, which gives what it used). The wait never
//! reaps, so the one reap is the one that reports the child's status: a wait that reaped it
//! would leave the reap after it `ECHILD` and the exit unreported. It makes its calls only
//! through the [`Children`] it is given, one value that the calls may change.
//!
//! Its proof is the theorem [`a_reaped_child_is_reported`]: the real [`reap`] run against a
//! kernel that plays one child's world (`crate::world::children`) for every life the child
//! starts with, every moment its exit comes relative to the reap's calls and every choice of
//! the kernel's, with the plant ([`ReapPlant::ReapInWait`]) as its control. The simulation
//! test in `wait/tests.rs` stays for now: it also runs the reap through the direct driver,
//! which is that driver's test.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::{io, time::Duration};

use super::{Clock, Deadline, WaitError, Waited, parking::Still};
use crate::world::children::{Child, Life, NoChild};

/// The kernel's children.
pub(crate) trait Children {
    /// What a reap says of a child: its status, and what it used.
    type Reaped;

    /// Reaps `pid` if it has exited, without blocking (`wait4(WNOHANG)`): `None` while it
    /// runs; an error (`ECHILD`) for a pid that is no child, or one reaped already.
    fn reap_now(&mut self, pid: i32) -> io::Result<Option<Self::Reaped>>;

    /// Waits until `pid` has exited, by `deadline` on `clock` (forever for `None`); with
    /// `keep`, leaves it unreaped (`WNOWAIT`).
    fn exited<C: Clock>(
        &mut self,
        what: &'static str,
        pid: i32,
        keep: bool,
        deadline: Option<Deadline>,
        clock: &C,
    ) -> Result<Waited<()>, WaitError>;
}

/// A safeguard of the reap a control removes, to see that the oracle catches the removal.
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
    kernel: &mut K,
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
        None => Err(WaitError::ran(what, pid)),
    }
}

/// The kernel of [`a_reaped_child_is_reported`]: one child's world, its exit coming where the
/// script says relative to the reap's calls (a reap makes at most three), and the kernel's one
/// choice. A wait with no deadline for a child that never exits never returns: that is no run,
/// and the kernel sets `stopped` and ends the wait to say so.
struct Script {
    child: Child,
    /// Where the child still runs after the reap's first look: it exits with this status
    /// before the wait begins.
    between: Option<i32>,
    /// Where it still runs when the wait begins: it exits with this status during the wait.
    during: Option<i32>,
    /// Where it exits during a wait that has a deadline: whether the wait is told the exit
    /// (true) or its deadline (false). The kernel has that choice only then.
    told: bool,
    /// A wait with no deadline for a child that never exits never returns: not a run. Set
    /// there.
    stopped: bool,
}

/// What a call about no child fails with. `ECHILD` is opaque to a proof: no run the theorem
/// claims reaches it.
fn no_child() -> io::Error {
    io::Error::other("no child")
}

impl Script {
    /// The wait looks, after the child exited: told it exited, and (without `keep`) reaping
    /// it, as a call made after the event is ready.
    fn looked(&mut self, what: &'static str, keep: bool) -> Result<Waited<()>, WaitError> {
        match self.child.waited(keep) {
            Ok(_) => Ok(Waited::Ready(())),
            Err(NoChild) => Err(WaitError::io(what, no_child())),
        }
    }
}

impl Children for Script {
    type Reaped = i32;

    fn reap_now(&mut self, _pid: i32) -> io::Result<Option<i32>> {
        match self.child.reap_now() {
            Ok(reaped) => Ok(reaped),
            Err(NoChild) => Err(no_child()),
        }
    }

    fn exited<C: Clock>(
        &mut self,
        what: &'static str,
        _pid: i32,
        keep: bool,
        deadline: Option<Deadline>,
        _clock: &C,
    ) -> Result<Waited<()>, WaitError> {
        match self.between {
            Some(status) => {
                self.child.exit(status);
                self.between = None;
            }
            None => {}
        }
        match self.child.life {
            Life::Reaped => Err(WaitError::io(what, no_child())),
            Life::Exited(_) => self.looked(what, keep),
            Life::Running => match self.during {
                Some(status) => {
                    self.child.exit(status);
                    if deadline.is_none() || self.told {
                        self.looked(what, keep)
                    } else {
                        // Told its deadline: the child stays a zombie.
                        Ok(Waited::Deadline)
                    }
                }
                None => {
                    if deadline.is_none() {
                        self.stopped = true;
                    }
                    Ok(Waited::Deadline)
                }
            },
        }
    }
}

/// The reap run with `plant` against the script of a child that starts as `life`: the script
/// after it, and how the reap ended.
fn run(
    plant: ReapPlant,
    life: Life,
    between: Option<i32>,
    during: Option<i32>,
    told: bool,
    deadline: Option<Deadline>,
) -> (Script, Result<Waited<i32>, WaitError>) {
    let mut script = Script { child: Child { life }, between, during, told, stopped: false };
    let reaped = reap("a child", &mut script, 7, deadline, &Still(Duration::ZERO), plant);
    (script, reaped)
}

/// Whether the reap, with `plant`, of a child that starts as `life`, exits `between` the
/// reap's first look and its wait or `during` the wait, its kernel choosing `told`, by
/// `deadline`, ends as it must. A pid that is no child, and a run that stopped, are none of
/// the theorem's (true). Of every other: the reap does not fail (in particular the wait never
/// reaps the child and leaves the reap `ECHILD`, its exit unreported); a reap that says ready
/// has reaped the child and reports the status it exited with; one that says its deadline
/// passed had one, leaves the child unreaped (running, or a zombie the next reap reports), and
/// found it running when its wait began.
fn reaps(
    plant: ReapPlant,
    life: Life,
    between: Option<i32>,
    during: Option<i32>,
    told: bool,
    deadline: Option<Deadline>,
) -> bool {
    let running = match life {
        Life::Reaped => return true,
        Life::Running => true,
        Life::Exited(_) => false,
    };
    // The status it exits with, where it exits.
    let status = match life {
        Life::Exited(status) => Some(status),
        Life::Running | Life::Reaped => match between {
            Some(status) => Some(status),
            None => during,
        },
    };
    let (script, reaped) = run(plant, life, between, during, told, deadline);
    if script.stopped {
        return true;
    }
    let reaped_now = match script.child.life {
        Life::Reaped => true,
        Life::Running | Life::Exited(_) => false,
    };
    match reaped {
        Err(_) => false,
        Ok(Waited::Ready(reported)) => {
            let same = match status {
                Some(status) => status == reported,
                None => false,
            };
            reaped_now && same
        }
        Ok(Waited::Deadline) => {
            let exited_before = match between {
                Some(_) => true,
                None => false,
            };
            deadline.is_some() && !reaped_now && running && !exited_before
        }
    }
}

/// The invariant (proved: crates/blaze-core/lean/BlazeCore/Wait/Reaping.lean): every child
/// reaped is reported with its status, by the one call that reaps
/// it; a reap that ends at its deadline leaves the child for the next reap; and a child's reap
/// never fails (no wait reaps it and leaves the reap after it `ECHILD`, its exit unreported).
/// For every life the child starts with (running, or exited with any status), every moment its
/// exit comes relative to the reap's calls (before the first look, between it and the wait,
/// during the wait, or never), every choice of the kernel's (told the exit or the deadline,
/// where both come in one wait) and every deadline or none, the real [`reap`] against one
/// child's world ([`Script`], `crate::world::children`) ends as [`reaps`] says. Its control is
/// `the_oracle_catches_a_child_reaped_in_the_wait`.
#[cfg_attr(not(test), expect(dead_code, reason = "a theorem function: `blaze prove` reads it"))]
#[blaze::theorem]
pub(crate) fn a_reaped_child_is_reported(
    running: bool,
    exited: i32,
    between: Option<i32>,
    during: Option<i32>,
    told: bool,
    deadline: Option<Duration>,
) -> bool {
    let life = match running {
        true => Life::Running,
        false => Life::Exited(exited),
    };
    let deadline = match deadline {
        Some(at) => Some(Deadline::at(at)),
        None => None,
    };
    reaps(ReapPlant::None, life, between, during, told, deadline)
}

#[cfg(test)]
mod theorems {
    use blaze_sim::{Broke, Config, check};

    use super::*;

    /// The theorem's function holds where it is run, on drawn lives, exits, choices and
    /// deadlines, and the draws reach each way a reap ends.
    #[test]
    fn a_reaped_child_is_reported_where_it_is_run() {
        let config = Config {
            must_reach: &[
                "reaped at the first look",
                "reaped after the wait",
                "ended at its deadline with the child running",
                "ended at its deadline with the child a zombie",
                "a run that stopped",
            ],
            ..Config::cases(1_000)
        };
        check(config, |tape| {
            let running = tape.coin("it runs at the first look");
            let exited = tape.int_in(0..=255i32, "its status");
            let between = tape.option("it exits before the wait", |tape| tape.int_in(0..=255i32, "status"));
            let during = tape.option("it exits during the wait", |tape| tape.int_in(0..=255i32, "status"));
            let told = tape.coin("the kernel tells the exit");
            let deadline =
                tape.option("a deadline", |tape| Duration::from_nanos(tape.int_in(0..=1_000u64, "deadline")));
            tape.note(format_args!(
                "running {running} exited {exited} between {between:?} during {during:?} told {told} deadline {deadline:?}"
            ));
            // The same run, to see how it ended.
            let life = match running {
                true => Life::Running,
                false => Life::Exited(exited),
            };
            let (script, reaped) =
                run(ReapPlant::None, life, between, during, told, deadline.map(Deadline::at));
            match (script.stopped, &reaped, script.child.life) {
                (true, _, _) => tape.reached("a run that stopped"),
                (false, Ok(Waited::Ready(_)), _) if !running => tape.reached("reaped at the first look"),
                (false, Ok(Waited::Ready(_)), _) => tape.reached("reaped after the wait"),
                (false, Ok(Waited::Deadline), Life::Running) => {
                    tape.reached("ended at its deadline with the child running");
                }
                (false, Ok(Waited::Deadline), Life::Exited(_)) => {
                    tape.reached("ended at its deadline with the child a zombie");
                }
                (false, Ok(Waited::Deadline), Life::Reaped) | (false, Err(_), _) => {}
            }
            match a_reaped_child_is_reported(running, exited, between, during, told, deadline) {
                true => Ok(()),
                false => Err(Broke::Oracle(format!(
                    "the reap ended {reaped:?}, the child then {:?}",
                    script.child.life
                ))),
            }
        });
    }

    /// The planted bug: the wait reaps the child (no `WNOWAIT`). A child that runs at the
    /// first look and exits during the wait, the kernel telling the exit, is reaped by the
    /// wait; the reap after it finds no child and fails, its exit unreported, which [`reaps`]
    /// refuses. Without the plant the same run reports it, with or without a deadline.
    #[test]
    fn the_oracle_catches_a_child_reaped_in_the_wait() {
        for deadline in [None, Some(Deadline::at(Duration::from_nanos(10)))] {
            let ran = |plant| reaps(plant, Life::Running, None, Some(3), true, deadline);
            assert!(ran(ReapPlant::None), "{deadline:?}");
            assert!(!ran(ReapPlant::ReapInWait), "{deadline:?}");
        }
    }

    /// The error of a wait that said the child exited and a reap after it that found it
    /// running names the wait and the pid, and wraps no system call's error.
    #[test]
    fn a_child_found_running_after_its_wait_is_a_typed_error() {
        let error = WaitError::ran("a child", 7);
        assert!(error.io_error().is_none());
        assert_eq!(
            error.to_string(),
            "waiting for a child: process 7: the wait said it exited, and the reap after found it running"
        );
    }
}
