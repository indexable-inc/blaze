//! Explicit `#![blaze::sans_io]` process-lifecycle machine for executing a build script binary
//! over typed linear process requests (`bstd::procs::req`).
//!
//! Lifecycle:
//! 1. [`Phase::Spawning`] — submits [`SpawnReq`] to start the build script process.
//! 2. [`Phase::Running`] — holds the linear [`ChildHandle`] while the child is live; normal
//!    progression transitions to [`Phase::Reaping`] via `WaitExitReq`, while external
//!    cancellation (`on_cancel`) transitions to [`Phase::Cancelling`] by emitting
//!    `KillReq` (`Signal::Terminate`) under a bounded [`bstd::machine::GraceBudget`].
//! 3. [`Phase::Cancelling`] — awaits `KillReq` completion and then unconditionally consumes the
//!    linear [`ChildHandle`] into `WaitExitReq` (`Phase::Reaping`) so no child handle leaks.
//! 4. [`Phase::Reaping`] / [`Phase::Done`] — checks the reaped [`ExitCode`], returning
//!    [`BuildScriptExecOutcome`] on `ExitCode::Code(0)` or a typed [`BuildScriptMachineError`].

#![blaze::sans_io]
#![forbid(unsafe_code)]

use core::fmt;

use blaze_own::planted::{NoPlant, Planted, Plants};
use bstd::machine::{CancelAction, CancelReason, Machine, Stopped, Turn};
use bstd::procs::Signal;
use bstd::procs::req::{
    ChildHandle, ExitCode, ProcsAns, ProcsReq, ProcsReqError, SpawnOpts, SpawnReq,
};

/// Planted-bug switches for [`BuildScriptExecMachine`] controls (rules 43 & 53).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum BuildScriptPlant {
    #[default]
    None,
    /// Planted bug: on external cancellation of a live child, skip `KillReq(Signal::Terminate)`
    /// and wait directly without terminating the child process.
    SkipKillOnCancel,
}

impl Plants for BuildScriptPlant {}

/// Terminal success outcome of [`BuildScriptExecMachine`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BuildScriptExecOutcome {
    pub exit: ExitCode,
}

/// Typed failure of [`BuildScriptExecMachine`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuildScriptMachineError {
    /// Process request handler failed (`NotFound`, `TableFull`, `StaleHandle`, ...).
    Process(ProcsReqError),
    /// Build script exited with a non-zero exit code or terminating signal.
    NonZeroExit { exit: ExitCode },
    /// Build script execution was cancelled externally and cleanly terminated/reaped.
    Cancelled { reason: CancelReason, exit: Option<ExitCode> },
}

impl fmt::Display for BuildScriptMachineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            BuildScriptMachineError::Process(err) => write!(f, "build script process error: {err}"),
            BuildScriptMachineError::NonZeroExit { exit } => {
                write!(f, "build script failed with {exit}")
            }
            BuildScriptMachineError::Cancelled { reason, exit: Some(exit) } => {
                write!(f, "build script cancelled ({reason:?}), reaped with {exit}")
            }
            BuildScriptMachineError::Cancelled { reason, exit: None } => {
                write!(f, "build script cancelled ({reason:?})")
            }
        }
    }
}

impl core::error::Error for BuildScriptMachineError {}

/// Explicit lifecycle phase of [`BuildScriptExecMachine`].
#[derive(Debug)]
pub enum Phase<'io> {
    /// Initial state or `SpawnReq` currently in flight.
    Spawning,
    /// Child process is running and owned by this machine until waited or cancelled.
    Running(ChildHandle<'io>),
    /// Cancellation grace path: `KillReq(Signal::Terminate)` is in flight while the linear
    /// [`ChildHandle`] is retained for the mandatory follow-up `WaitExitReq`.
    Cancelling(ChildHandle<'io>),
    /// `WaitExitReq` is in flight (linear [`ChildHandle`] has been moved to the handler).
    Reaping,
    /// Terminal state after completion or failure.
    Done,
}

/// Explicit `#![blaze::sans_io]` `bstd::machine::Machine` driving a build script process through
/// typed linear `bstd::procs::req::ProcsReq` requests with bounded graceful cancellation.
#[derive(Debug)]
pub struct BuildScriptExecMachine<'io, 'a, P: Planted<BuildScriptPlant> = NoPlant> {
    program: &'a str,
    args: &'a [&'a str],
    env: &'a [(&'a str, &'a str)],
    phase: Phase<'io>,
    cancel_reason: Option<CancelReason>,
    plant: P,
}

impl<'io, 'a> BuildScriptExecMachine<'io, 'a, NoPlant> {
    /// Creates a production [`BuildScriptExecMachine`] ready to spawn `opts`.
    #[inline]
    pub const fn new(opts: SpawnOpts<'a>) -> Self {
        Self {
            program: opts.program,
            args: opts.args,
            env: opts.env,
            phase: Phase::Spawning,
            cancel_reason: None,
            plant: NoPlant,
        }
    }

    /// Creates a [`BuildScriptExecMachine`] already owning a live [`ChildHandle`] in
    /// [`Phase::Running`].
    #[inline]
    pub const fn from_running(child: ChildHandle<'io>) -> Self {
        Self {
            program: "",
            args: &[],
            env: &[],
            phase: Phase::Running(child),
            cancel_reason: None,
            plant: NoPlant,
        }
    }
}

impl<'io, 'a, P: Planted<BuildScriptPlant>> BuildScriptExecMachine<'io, 'a, P> {
    /// Creates a [`BuildScriptExecMachine`] carrying a planted-bug seam for controls.
    #[inline]
    pub const fn with_plant(opts: SpawnOpts<'a>, plant: P) -> Self {
        Self {
            program: opts.program,
            args: opts.args,
            env: opts.env,
            phase: Phase::Spawning,
            cancel_reason: None,
            plant,
        }
    }

    /// Creates a [`BuildScriptExecMachine`] in [`Phase::Running`] carrying a planted-bug seam.
    #[inline]
    pub const fn with_plant_running(child: ChildHandle<'io>, plant: P) -> Self {
        Self {
            program: "",
            args: &[],
            env: &[],
            phase: Phase::Running(child),
            cancel_reason: None,
            plant,
        }
    }

    /// Inspects the machine's current [`Phase`].
    #[inline]
    pub const fn phase(&self) -> &Phase<'io> {
        &self.phase
    }
}

impl<'io, 'a, P: Planted<BuildScriptPlant>> Machine<()> for BuildScriptExecMachine<'io, 'a, P> {
    type Req = ProcsReq<'io, 'a>;
    type Out = BuildScriptExecOutcome;
    type Err = BuildScriptMachineError;

    fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        let Self { program, args, env, phase, cancel_reason, plant } = self;
        match phase {
            Phase::Spawning => Turn::Ask {
                request: ProcsReq::Spawn(SpawnReq::new(SpawnOpts { program, args, env })),
                next: Self {
                    program,
                    args,
                    env,
                    phase: Phase::Spawning,
                    cancel_reason,
                    plant,
                },
            },
            Phase::Running(child) | Phase::Cancelling(child) => Turn::Ask {
                request: ProcsReq::WaitExit(child.wait_exit()),
                next: Self {
                    program,
                    args,
                    env,
                    phase: Phase::Reaping,
                    cancel_reason,
                    plant,
                },
            },
            Phase::Reaping | Phase::Done => {
                unreachable!("BuildScriptExecMachine::start called after execution already began")
            }
        }
    }

    fn resume(
        self,
        answer: ProcsAns<'io>,
        _cx: &mut (),
    ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        let Self { program, args, env, phase, cancel_reason, plant } = self;
        match (phase, answer) {
            (Phase::Spawning, ProcsAns::Spawned(Ok(child))) => Turn::Ask {
                request: ProcsReq::WaitExit(child.wait_exit()),
                next: Self {
                    program,
                    args,
                    env,
                    phase: Phase::Reaping,
                    cancel_reason,
                    plant,
                },
            },
            (Phase::Spawning, ProcsAns::Spawned(Err(err))) => {
                Turn::Fail(BuildScriptMachineError::Process(err))
            }
            // Regardless of whether `KillReq` succeeded or raced with a natural exit
            // (`AlreadyExited`), unconditionally reap the linear handle via `WaitExitReq`.
            (Phase::Cancelling(child), ProcsAns::Killed(_)) => Turn::Ask {
                request: ProcsReq::WaitExit(child.wait_exit()),
                next: Self {
                    program,
                    args,
                    env,
                    phase: Phase::Reaping,
                    cancel_reason,
                    plant,
                },
            },
            (Phase::Reaping, ProcsAns::Exited(Ok(exit))) => {
                if let Some(reason) = cancel_reason {
                    Turn::Fail(BuildScriptMachineError::Cancelled { reason, exit: Some(exit) })
                } else if exit.is_success() {
                    Turn::Done(BuildScriptExecOutcome { exit })
                } else {
                    Turn::Fail(BuildScriptMachineError::NonZeroExit { exit })
                }
            }
            (Phase::Reaping, ProcsAns::Exited(Err(err))) => {
                if let Some(reason) = cancel_reason {
                    Turn::Fail(BuildScriptMachineError::Cancelled { reason, exit: None })
                } else {
                    Turn::Fail(BuildScriptMachineError::Process(err))
                }
            }
            _ => unreachable!("BuildScriptExecMachine received mismatched ProcsAns for active Phase"),
        }
    }

    fn on_cancel(
        self,
        stopped: Stopped,
        _cx: &mut (),
    ) -> CancelAction<Turn<Self, Self::Req, Self::Out, Self::Err>> {
        let Self { program, args, env, phase, cancel_reason, plant } = self;
        match phase {
            Phase::Running(child) => {
                if !plant.has(BuildScriptPlant::SkipKillOnCancel) && stopped.grace.max_steps >= 2 {
                    let kill = child.kill(Signal::Terminate);
                    CancelAction::Cleanup(Turn::Ask {
                        request: ProcsReq::Kill(kill),
                        next: Self {
                            program,
                            args,
                            env,
                            phase: Phase::Cancelling(child),
                            cancel_reason: Some(stopped.reason),
                            plant,
                        },
                    })
                } else {
                    CancelAction::Cleanup(Turn::Ask {
                        request: ProcsReq::WaitExit(child.wait_exit()),
                        next: Self {
                            program,
                            args,
                            env,
                            phase: Phase::Reaping,
                            cancel_reason: Some(stopped.reason),
                            plant,
                        },
                    })
                }
            }
            Phase::Cancelling(child) => CancelAction::Cleanup(Turn::Ask {
                request: ProcsReq::WaitExit(child.wait_exit()),
                next: Self {
                    program,
                    args,
                    env,
                    phase: Phase::Reaping,
                    cancel_reason: Some(cancel_reason.unwrap_or(stopped.reason)),
                    plant,
                },
            }),
            Phase::Spawning | Phase::Reaping | Phase::Done => CancelAction::Abandon,
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::{Alloc, testing::heap, with_brand};
    use bstd::machine::{
        CancelAction, CancelReason, DriveError, DriveOpts, GraceBudget, Handler, Machine, Perform,
        Stopped, Turn, drive,
    };
    use bstd::procs::Signal;
    use bstd::procs::req::{
        ExitCode, ProcsReq, ProcsReqError, SimProcsHandler, SimProcsOpts, SpawnOpts, SpawnReq,
    };

    use super::{
        BuildScriptExecMachine, BuildScriptExecOutcome, BuildScriptMachineError, BuildScriptPlant,
        Phase,
    };

    fn run_cancel_with_plant(
        plant: BuildScriptPlant,
        alloc: Alloc<'static>,
    ) -> (BuildScriptMachineError, Option<Signal>) {
        let mut out_err = None;
        let mut out_sig = None;

        with_brand(|brand| {
            let mut handler =
                SimProcsHandler::new(SimProcsOpts { brand, max_children: 4, alloc });
            handler
                .register("/build/script", ExitCode::Code(0))
                .expect("register build script");

            let child = match Handler::perform(
                &mut handler,
                SpawnReq::new(SpawnOpts {
                    program: "/build/script",
                    args: &["--release"],
                    env: &[("OUT_DIR", "/target/out")],
                }),
                core::convert::identity,
            )
            .expect("infallible handler")
            {
                Perform::Answered(res) => res.expect("spawn ok"),
                Perform::Stopped(_) => unreachable!("SimProcsHandler does not stop"),
            };
            let child_slot = child.slot();
            assert_eq!(handler.live_children(), 1);

            let machine = BuildScriptExecMachine::with_plant_running(child, plant);
            assert!(matches!(machine.phase(), Phase::Running(_)));

            let stopped = Stopped::cancelled(GraceBudget::DEFAULT);
            let mut turn = match machine.on_cancel(stopped, &mut ()) {
                CancelAction::Cleanup(cleanup_turn) => cleanup_turn,
                CancelAction::Abandon => panic!("Running child must enter bounded cleanup"),
            };

            let mut steps_left = stopped.grace.max_steps;
            let err = loop {
                match turn {
                    Turn::Fail(err) => break err,
                    Turn::Done(done) => panic!("cancelled build script returned Done({done:?})"),
                    Turn::Ask { request, next } => {
                        assert!(steps_left > 0, "cancellation cleanup exhausted grace budget");
                        steps_left -= 1;
                        if matches!(request, ProcsReq::Kill(_)) {
                            assert!(matches!(next.phase(), Phase::Cancelling(_)));
                        }
                        turn = match Handler::perform(&mut handler, request, core::convert::identity)
                            .expect("infallible handler")
                        {
                            Perform::Answered(ans) => next.resume(ans, &mut ()),
                            Perform::Stopped(_) => unreachable!("SimProcsHandler does not stop"),
                        };
                    }
                }
            };

            assert_eq!(handler.finish_check(), Ok(()), "child handle must be reaped with zero leaks");
            let sig = handler
                .signals()
                .iter()
                .find(|&&(slot, _)| slot == child_slot)
                .map(|&(_, sig)| sig);
            out_err = Some(err);
            out_sig = sig;
        });

        (out_err.expect("cancellation produced typed error"), out_sig)
    }

    #[test]
    fn build_script_exec_normal_exit_zero_succeeds_with_zero_leaks() {
        let h = heap();
        let alloc = h.alloc();

        with_brand(|brand| {
            let mut handler =
                SimProcsHandler::new(SimProcsOpts { brand, max_children: 4, alloc });
            handler
                .register("/build/ok-script", ExitCode::Code(0))
                .expect("register ok script");

            let machine = BuildScriptExecMachine::new(SpawnOpts {
                program: "/build/ok-script",
                args: &["--emit"],
                env: &[("CARGO_PKG_NAME", "demo")],
            });
            let outcome = drive(&mut handler, &mut (), machine, DriveOpts { max_steps: 8 });
            assert_eq!(outcome, Ok(BuildScriptExecOutcome { exit: ExitCode::Code(0) }));
            assert!(handler.signals().is_empty());
            assert_eq!(handler.finish_check(), Ok(()));
        });
    }

    #[test]
    fn build_script_exec_non_zero_exit_fails_typed_with_zero_leaks() {
        let h = heap();
        let alloc = h.alloc();

        with_brand(|brand| {
            let mut handler =
                SimProcsHandler::new(SimProcsOpts { brand, max_children: 4, alloc });
            handler
                .register("/build/fail-script", ExitCode::Code(1))
                .expect("register fail script");

            let machine = BuildScriptExecMachine::new(SpawnOpts {
                program: "/build/fail-script",
                args: &[],
                env: &[],
            });
            let res = drive(&mut handler, &mut (), machine, DriveOpts { max_steps: 8 });
            assert_eq!(
                res,
                Err(DriveError::Machine(BuildScriptMachineError::NonZeroExit {
                    exit: ExitCode::Code(1),
                }))
            );
            assert_eq!(handler.finish_check(), Ok(()));

            // Missing binary returns typed Process(NotFound) with zero leaks.
            let missing = BuildScriptExecMachine::new(SpawnOpts {
                program: "/build/missing-script",
                args: &[],
                env: &[],
            });
            let missing_res = drive(&mut handler, &mut (), missing, DriveOpts { max_steps: 8 });
            assert_eq!(
                missing_res,
                Err(DriveError::Machine(BuildScriptMachineError::Process(
                    ProcsReqError::NotFound,
                )))
            );
            assert_eq!(handler.finish_check(), Ok(()));
        });
    }

    #[test]
    fn build_script_exec_cancel_kills_and_reaps_with_planted_control() {
        let h = heap();
        let alloc = h.alloc();

        // Clean machine: `on_cancel` sends `KillReq(Signal::Terminate)` -> `WaitExitReq`,
        // terminating and reaping the child cleanly with zero leaked handles.
        let (clean_err, clean_sig) = run_cancel_with_plant(BuildScriptPlant::None, alloc);
        assert_eq!(clean_sig, Some(Signal::Terminate));
        assert_eq!(
            clean_err,
            BuildScriptMachineError::Cancelled {
                reason: CancelReason::Cancelled,
                exit: Some(ExitCode::Signaled(Signal::Terminate)),
            }
        );

        // Planted-bug control: `SkipKillOnCancel` omits `KillReq(Signal::Terminate)`, so no signal
        // is delivered and the child exits normally instead of `Signaled(Signal::Terminate)`.
        let (planted_err, planted_sig) =
            run_cancel_with_plant(BuildScriptPlant::SkipKillOnCancel, alloc);
        assert_eq!(planted_sig, None);
        assert_eq!(
            planted_err,
            BuildScriptMachineError::Cancelled {
                reason: CancelReason::Cancelled,
                exit: Some(ExitCode::Code(0)),
            }
        );
    }
}
