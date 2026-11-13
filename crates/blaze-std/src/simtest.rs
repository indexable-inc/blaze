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
//!   macro. It turns the function into a `#[test]` that calls [`run`] with its package, its
//!   path and its seeds.
//!
//! A test that needs a world of its own making (scripted devices, an environment, a withheld
//! capability) is not this attribute's yet: it makes its `Io` and loops its seeds, as the
//! tests of this crate do.

use alloc::{boxed::Box, format, string::String, string::ToString};
use core::{fmt, panic::AssertUnwindSafe, time::Duration};

use blaze_core::sim::{
    KILLED, Sim,
    seeds::{self, Ended, Failed, Given, Name, Passed, Run, Seeds, World},
};

use crate::{
    cx::Cx,
    host::Exit,
    io::Io,
    mem::TASK_FRAME,
    rt::{catch_unwind, resume_unwind},
    task::said,
};

/// The budget of a start's root context, on the simulation's clock: an hour, longer than any
/// wait a test means to finish, so the deadlines a test sets are the ones that end its waits.
pub const BUDGET: Duration = Duration::from_secs(3600);

/// How a test's program ended, by what it returned.
pub trait Outcome {
    /// `None` when all is well, else the error it gave up with, as it says itself.
    fn gave_up(self) -> Option<String>;
}

impl Outcome for () {
    fn gave_up(self) -> Option<String> {
        None
    }
}

impl<T, E: fmt::Display> Outcome for Result<T, E> {
    fn gave_up(self) -> Option<String> {
        // alloc: an error's message.
        self.err().map(|error| error.to_string())
    }
}

/// A simulated world and the program a test runs in it.
struct Program<'p, P> {
    io: Io,
    program: &'p P,
}

impl<P, R> World for Program<'_, P>
where
    P: Fn(&Cx, Io) -> R,
    R: Outcome,
{
    fn run(&mut self) -> Ended {
        let cx = match self.io.cx(BUDGET, TASK_FRAME) {
            Ok(cx) => cx,
            // alloc: a failure's message.
            Err(stopped) => return Ended::Panicked(format!("bstd::test: a start's root context: {stopped}")),
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
                Ok(_) if self.with_sim(|sim| sim.is_off()) => Ended::PowerLost,
                Ok(exit) => match exit.status {
                    0 => Ended::Well,
                    // alloc: an error's message.
                    status => Ended::GaveUp(format!("the process exited with status {status}")),
                },
                // alloc: a panic's message, for the seed's report.
                Err(payload) => Ended::Panicked(said(payload).into_owned()),
            },
        }
    }

    fn crash(&mut self) {
        self.io.crash();
    }

    fn with_sim<T>(&mut self, f: impl FnOnce(&mut Sim) -> T) -> T {
        self.io.with_sim(f).expect("a bstd::test's world is Io::sim's: simulated")
    }
}

/// The armed simulation cut the power at the effect just handed to it ([`Sim::cut_due`]): the
/// world crashes and the calling task is unwound with an [`Exit`] payload, which a scope
/// passes on, so the program's start ends here. Called by [`Io`]'s submit, its lock let go.
pub(crate) fn power_lost(io: &Io) -> ! {
    io.crash();
    // alloc: once a cut, the unwind's payload.
    resume_unwind(Box::new(Exit { status: KILLED }))
}

/// The name the test harness lists a test by: its module's path without the crate's name,
/// then its own.
fn listed(module: &str, test: &str) -> String {
    match module.split_once("::") {
        // alloc: a failure's name of the test.
        Some((_, path)) => format!("{path}::{test}"),
        None => test.to_string(),
    }
}

/// `program` under `seeds`, or under the seed or the count `given` in their place: what
/// [`run`] runs, as a value. `package` is the test's package and `module` and `test` its path
/// (`module_path!()` and the function's name), for what a failure says and the line that
/// replays it.
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
    let listed = listed(module, test);
    let name = Name { package, test: &listed };
    seeds::run(seeds, given, name, |seed| Program { io: Io::sim(seed), program: &program })
}

/// One seed of `program` under `seeds`: its verdict, its counts and its trace.
pub fn one<P, R>(seed: u64, seeds: Seeds, program: P) -> Run
where
    P: Fn(&Cx, Io) -> R,
    R: Outcome,
{
    seeds::one(seed, &seeds, &mut |seed| Program { io: Io::sim(seed), program: &program })
}

/// What `#[bstd::test]` expands to: `program` under `seeds`, or under what the person running
/// the test gave in the environment ([`seeds::SEED_VAR`], [`seeds::SEEDS_VAR`]); a failure
/// panics with its report, which the test harness prints.
pub fn run<P, R>(package: &str, module: &str, test: &str, seeds: Seeds, program: P)
where
    P: Fn(&Cx, Io) -> R,
    R: Outcome,
{
    let failed = match Given::from_env() {
        Ok(given) => match seeded(package, module, test, seeds, given, program) {
            Ok(_) => return,
            Err(failed) => failed,
        },
        Err(why) => Failed::Given(why),
    };
    panic!("bstd::test: {failed}");
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
        $vis fn $name() {
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
