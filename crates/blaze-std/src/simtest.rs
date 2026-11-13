//! `#[bstd::test]`: a test shaped like `main` (`fn(cx: &Cx, io: Io)`), run on the simulation
//! under many seeds with its faults on (docs/v2/plan/design/bstd-sim-test.md). The simulated
//! world is the default of a bstd test, as TigerBeetle's VOPR is of TigerBeetle's.
//!
//! ```ignore
//! #[bstd::test]
//! fn the_ledger_keeps_what_it_committed(cx: &Cx, io: Io) -> Result<(), FsError> { .. }
//!
//! #[bstd::test(without(Crashes))]          // a fault kind is left out by name
//! fn a_report_is_rendered(cx: &Cx, io: Io) { .. }
//! ```
//!
//! The runner is blaze-core's ([`blaze_core::sim::seeds`], re-exported in [`crate::test`]):
//! 256 seeds, each a world of its own ([`Io::sim`]) armed with what the seed injects (failed
//! effects, late completions, power cuts); the program started again after each cut and each
//! error it gives up with; a last start once the faults are healed, which has to end well. A
//! failure says its seed, its plan, the end of its trace and the one line that replays it,
//! and whether its second run was the same run, byte for byte.
//!
//! What this module adds to the runner:
//!
//! - [`run`] and [`seeded`]: the runner over a program on an [`Io`]. A start makes a root
//!   context of [`BUDGET`] and [`TASK_FRAME`] and calls the program with it and the world, as
//!   the process's entry calls `main`. How it ended is its [`Outcome`]: `()` is well, a
//!   `Result`'s error is the program giving up (restarted under faults, a failure once they
//!   are healed), a panic fails the seed.
//! - [`power_lost`]: the unwind of a power cut. [`Io`]'s submit asks the armed simulation
//!   after each effect; on a cut the world crashes ([`Io::crash`], the disk's ledger with it)
//!   and the program is unwound with an [`Exit`] payload, as the simulated exit unwinds it and
//!   as scopes pass on. Nothing of the program runs after the cut but its destructors, and
//!   the machine is off: what they ask is dropped.
//! - The attribute, [`crate::test!`]: a `macro_rules!` attribute rule (`macro_attr`), no proc
//!   macro. It turns the function into a `#[test]` that returns what [`run`] does with its
//!   package, its path and its seeds: a failure is the test's `Err`, a [`Report`], which the
//!   test harness prints.
//!
//! Nothing here panics or allocates infallibly (decision D29): a world that cannot be made
//! is its seed's failure, naming the seed and why; the simulation is steered through its
//! own handle ([`Io::steered`]), so no start asks whether its world is simulated; a power
//! cut that finds no room to unwind with is the submit's [`Stopped`]; and a message with no
//! room on the global heap is a fixed one saying so.
//!
//! A test that needs a world of its own making (scripted devices, an environment, a withheld
//! capability) is not this attribute's yet: it makes its `Io` and loops its seeds, as the
//! tests of this crate do.

use alloc::borrow::Cow;
use core::{fmt, panic::AssertUnwindSafe, time::Duration};

use blaze_core::sim::{
    KILLED, Sim,
    seeds::{self, Ended, Failed, Given, Name, Passed, Run, Seeds, World},
};

use crate::{
    Plant, Stopped,
    cx::Cx,
    host::Exit,
    io::{Io, Limits, SimError, SimHandle, SimWorld},
    mem::TASK_FRAME,
    rt::{catch_unwind, resume_unwind},
    task::{message, said},
};

/// The budget of a start's root context, on the simulation's clock: an hour, longer than any
/// wait a test means to finish, so the deadlines a test sets are the ones that end its waits.
pub const BUDGET: Duration = Duration::from_secs(3600);

/// How a test's program ended, by what it returned.
pub trait Outcome {
    /// `None` when all is well, else the error it gave up with, as it says itself (or, with
    /// no room on the global heap for that, a fixed text saying so).
    fn gave_up(self) -> Option<Cow<'static, str>>;
}

impl Outcome for () {
    fn gave_up(self) -> Option<Cow<'static, str>> {
        None
    }
}

impl<T, E: fmt::Display> Outcome for Result<T, E> {
    fn gave_up(self) -> Option<Cow<'static, str>> {
        self.err().map(|error| message(&error))
    }
}

/// A simulated world, its simulation and the program a test runs in it.
struct Program<'p, P> {
    io: Io,
    /// The world's simulation itself: what the runner arms, heals and reads.
    sim: SimHandle,
    program: &'p P,
}

impl<'p, P> Program<'p, P> {
    /// The world of `seed`, as [`Io::sim`] makes it, with `program` to run in it.
    fn of(seed: u64, program: &'p P) -> Result<Program<'p, P>, SimError> {
        let (io, sim) = Io::steered(SimWorld::new(seed, Limits::USUAL), Plant::None)?;
        Ok(Program { io, sim, program })
    }
}

/// A start's failure before the program ran: no root context.
struct NoRoot(Stopped);

impl fmt::Display for NoRoot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::test: a start's root context: {}", self.0)
    }
}

/// A simulated exit with a status other than zero.
struct Exited(i32);

impl fmt::Display for Exited {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the process exited with status {}", self.0)
    }
}

impl<P, R> World for Program<'_, P>
where
    P: Fn(&Cx, Io) -> R,
    R: Outcome,
{
    fn run(&mut self) -> Ended {
        let cx = match self.io.cx(BUDGET, TASK_FRAME) {
            Ok(cx) => cx,
            Err(stopped) => return Ended::Panicked(message(&NoRoot(stopped))),
        };
        // clone: the program's own handle to its world, as `main` is handed one.
        let io = self.io.clone();
        let ran = catch_unwind(AssertUnwindSafe(|| (self.program)(&cx, io)));
        drop(cx);
        match ran {
            Ok(outcome) => match outcome.gave_up() {
                None => Ended::Well,
                Some(error) => Ended::GaveUp(error),
            },
            Err(payload) => match payload.downcast::<Exit>() {
                Ok(exit) => {
                    let status = exit.status;
                    // The payload goes back to the world: its next cut or exit unwinds with
                    // this one, so a world allocates a payload once.
                    self.io.keep_exit(exit);
                    match status {
                        _ if self.sim.lock().is_off() => Ended::PowerLost,
                        0 => Ended::Well,
                        status => Ended::GaveUp(message(&Exited(status))),
                    }
                }
                // A panic's message, moved out of its payload: nothing is copied.
                Err(payload) => Ended::Panicked(said(payload)),
            },
        }
    }

    fn crash(&mut self) {
        self.io.crash();
    }

    fn with_sim<T>(&mut self, f: impl FnOnce(&mut Sim) -> T) -> T {
        f(&mut self.sim.lock())
    }
}

/// The armed simulation cut the power at the effect just handed to it ([`Sim::cut_due`]): the
/// world crashes and the calling task is unwound with an [`Exit`] payload, which a scope
/// passes on, so the program's start ends here. Called by [`Io`]'s submit, its lock let go.
///
/// It returns only an error: the world's payload was out and there was no room for another.
/// The machine is off all the same (nothing the program asks reaches it), and the submit
/// that met the cut returns this to its caller.
pub(crate) fn power_lost(io: &Io) -> Result<!, Stopped> {
    io.crash();
    let payload = io.exit_payload(KILLED)?;
    resume_unwind(payload)
}

/// `program` under `seeds`, or under the seed or the count `given` in their place: what
/// [`run`] runs, as a value. `package` is the test's package and `module` and `test` its path
/// (`module_path!()` and the function's name), for what a failure says and the line that
/// replays it. A seed whose world cannot be made fails, naming the seed and why.
pub fn seeded<P, R>(
    package: &str,
    module: &str,
    test: &str,
    seeds: Seeds,
    given: Given,
    program: P,
) -> Result<Passed, Failed>
where
    P: Fn(&Cx, Io) -> R,
    R: Outcome,
{
    let name = Name { package, module, test };
    seeds::run(seeds, given, name, |seed| Program::of(seed, &program))
}

/// One seed of `program` under `seeds`: its verdict, its counts and its trace.
pub fn one<P, R>(seed: u64, seeds: Seeds, program: P) -> Run
where
    P: Fn(&Cx, Io) -> R,
    R: Outcome,
{
    seeds::one(seed, &seeds, &mut |seed| Program::of(seed, &program))
}

/// Why a `#[bstd::test]` failed, as the test harness prints a test's `Err`: the runner's
/// report (its seed, its plan, the end of its trace, the line that replays it).
pub struct Report(pub Failed);

impl fmt::Debug for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bstd::test: {}", self.0)
    }
}

impl core::error::Error for Report {}

/// What `#[bstd::test]` expands to: `program` under `seeds`, or under what the person running
/// the test gave in the environment ([`seeds::SEED_VAR`], [`seeds::SEEDS_VAR`]); a failure is
/// its [`Report`], which the test returns and the test harness prints.
pub fn run<P, R>(package: &str, module: &str, test: &str, seeds: Seeds, program: P) -> Result<(), Report>
where
    P: Fn(&Cx, Io) -> R,
    R: Outcome,
{
    let given = Given::from_env().map_err(|why| Report(Failed::Given(why)))?;
    match seeded(package, module, test, seeds, given, program) {
        Ok(_passed) => Ok(()),
        Err(failed) => Err(Report(failed)),
    }
}

/// The attribute of a simulation test (the module's doc): `#[bstd::test]` on a function
/// shaped like `main`, `fn(cx: &Cx, io: Io)`, returning `()` or a `Result`. A fault kind is
/// left out by name: `#[bstd::test(without(Crashes, IoFailures))]`
/// ([`crate::test::Fault`]).
#[macro_export]
macro_rules! test {
    attr($(without($($fault:ident),+ $(,)?))?) (
        $(#[$meta:meta])*
        $vis:vis fn $name:ident($($params:tt)*) $(-> $returns:ty)? $body:block
    ) => {
        $(#[$meta])*
        #[::core::prelude::v1::test]
        $vis fn $name() -> ::core::result::Result<(), $crate::test::Report> {
            fn $name($($params)*) $(-> $returns)? $body
            $crate::test::run(
                ::core::env!("CARGO_PKG_NAME"),
                ::core::module_path!(),
                ::core::stringify!($name),
                $crate::test::Seeds::DEFAULT $($(.without($crate::test::Fault::$fault))+)?,
                $name,
            )
        }
    };
}
