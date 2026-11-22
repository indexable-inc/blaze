//! `#[bstd::test]`: a test names what it takes in its signature, as `main` does (D104,
//! D109), and is run on the simulation under seeds, with the fault kinds its parameters can
//! meet on (docs/v2/plan/design/bstd-sim-test.md). The simulated world is the default of a
//! bstd test, as TigerBeetle's VOPR is of TigerBeetle's.
//!
//! ```ignore
//! #[bstd::test(data = files("ledger/head" => b"0\n"))]
//! fn the_ledger_keeps_what_it_committed(cx: &Cx, data: Dir<ReadWrite>, clock: Clock) -> Result<(), FsError> { .. }
//!
//! #[bstd::test(without(Crashes))]          // a fault kind its folder meets, left out by name
//! fn a_report_is_rendered(cx: &Cx, out: Out, data: Dir<ReadWrite>) { .. }
//! ```
//!
//! The runner is blaze-core's ([`blaze_core::sim::seeds`], re-exported in [`crate::test`]);
//! the test entry, its setup, the plan by parameter kind and the one-block report are
//! [`granted`]'s. What stays here:
//!
//! - [`Outcome`]: how a test's program ended, by what it returned: `()` is well, a `Result`'s
//!   error is the program giving up (restarted under faults, a failure once they are healed).
//! - [`power_lost`]: the unwind of a power cut. [`Io`]'s submit asks the armed simulation
//!   after each effect; on a cut the world crashes ([`Io::crash`], the disk's ledger with it)
//!   and the program is unwound with an [`Exit`] payload, as the simulated exit unwinds it and
//!   as scopes pass on. Nothing of the program runs after the cut but its destructors, and
//!   the machine is off: what they ask is dropped.
//! - [`seeded`]: the runner over a library under test that itself takes a whole [`Io`] (a
//!   store over `OnDir { io, dir }`), called from a `#[test]` with its own seeds. Not a test's
//!   form: a test names what it takes. It goes when those libraries take what they use (B1).
//! - The attribute, [`crate::test!`]: a `macro_rules!` attribute rule (`macro_attr`), no proc
//!   macro. A whole `Io` is refused at the attribute, naming what to write instead.

use alloc::{boxed::Box, format, string::String, string::ToString};
use core::{fmt, panic::AssertUnwindSafe, time::Duration};

use blaze_core::sim::{
    KILLED, Sim,
    seeds::{self, Ended, Failed, Given, Name, Passed, Seeds, World},
};

use crate::{
    cx::Cx,
    host::Exit,
    io::Io,
    mem::TASK_FRAME,
    rt::{catch_unwind, resume_unwind},
    task::said,
};

/// `#[bstd::test]` with grants (D109): the test entry, its setup and its report.
pub(crate) mod granted;
/// What makes one parameter of a test: a capability or a drawn value (D109).
pub(crate) mod param;
/// The drawn half's runner: a case is a world seed and a tape (D109).
#[cfg(any(test, feature = "sim-draws"))]
pub(crate) mod drawn;

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
                Err(payload) => Ended::Panicked(said(&*payload)),
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

/// `program`, a library under test that itself takes a whole `Io` (a store over
/// `OnDir { io, dir }`), under `seeds`, or under the seed or the count `given` in their place,
/// as a value. `package` is the test's package and `module` and `test` its path
/// (`module_path!()` and the function's name), for what a failure says and the line that
/// replays it. Not a test's form (a `#[bstd::test]` names what it takes, D109); it goes when
/// its callers' stores take what they use (batch B1).
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

/// The attribute of a simulation test (D109; [`granted`]'s doc): `#[bstd::test]` on a function
/// that names what it takes after `cx`, as `main` does, returning `()` or a `Result`:
///
/// ```ignore
/// #[bstd::test(data = files("ledger/head" => b"0\n"))]
/// fn the_ledger_keeps_what_it_committed(cx: &Cx, data: Dir<ReadWrite>, clock: Clock) -> Result<(), FsError> { .. }
/// ```
///
/// Each parameter is a [`crate::grant::Grant`] made from the test's simulated world, narrowed
/// to that one capability; a type that is not one is a compile error at its parameter. The
/// arguments, any order, each once, all checked as the crate compiles: `seed = 41` (one seed
/// alone, the one a failure named), `without(IoFailures, Delays, Crashes)` (a fault kind the
/// parameters can meet, left out), `real` (the machine, beneath a directory made for the
/// test), `<dir> = files("path" => b"..", "p2" => include_bytes!("f"))`, `<net> =
/// connect("host:port")` or `listen("host:port")`, `env(NAME = "value")`, `args("..")`,
/// `input = b".."`. The fault kinds a seed explores are those the parameters can meet; a test
/// that meets none (`cx` alone, `Out`, `Args`, `Env`) is run once. A failure is one block
/// ([`granted::report`]) ending in the one line that replays it.
///
/// A whole `io: Io` after `cx` is refused here, naming the parameters to write; a whole `Io`
/// anywhere else is refused at its type (`Io` is no `Grant`, D104).
#[macro_export]
macro_rules! test {
    // The whole world is no parameter (D109): refused with what to write.
    attr($($arg:tt)*) (
        $(#[$meta:meta])*
        $vis:vis fn $name:ident($cx:ident : $cxty:ty, $io:ident : Io $(, $($rest:tt)*)?) $(-> $returns:ty)? $body:block
    ) => {
        ::core::compile_error!(::core::concat!(
            "bstd::test: `", ::core::stringify!($name), "` takes a whole `", ::core::stringify!($io), ": Io`, ",
            "and a test cannot take a whole `Io` (D109): name what it uses after `cx`, each by its type, ",
            "e.g. `data: Dir<ReadWrite>, clock: Clock, out: Out, api: Net` ",
            "(a folder's files, an endpoint, the environment are set up in the attribute: ",
            "`#[bstd::test(data = files(\"a\" => b\"..\"), api = connect(\"host:443\"))]`)"
        ));
    };
    // A test that names its grants (D109).
    attr($($arg:tt)*) (
        $(#[$meta:meta])*
        $vis:vis fn $name:ident($cx:ident : $cxty:ty $(, $param:ident : $ty:ty)* $(,)?) $(-> $returns:ty)? $body:block
    ) => {
        // The attribute checked against the parameters as the crate compiles, in every build
        // (a `#[test]` function is compiled only under `--test`; this item always is).
        const _: $crate::test::Planned = {
            const SLOTS: &[$crate::test::Slot] = &[
                $( $crate::test::slot::<$ty>(::core::stringify!($param)) ),*
            ];
            const PARAMS: &[$crate::grant::Param] =
                &$crate::test::grants::<{ $crate::test::grant_count(SLOTS) }>(SLOTS);
            const SETUP: $crate::test::Setup =
                $crate::__test_setup!(@ $crate::test::Setup::DEFAULT ; $($arg)*).checked(PARAMS);
            $crate::test::Planned::of(&SETUP, PARAMS).drawing(&SETUP, $crate::test::drawn(SLOTS))
        };
        $(#[$meta])*
        #[::core::prelude::v1::test]
        $vis fn $name() {
            const SLOTS: &[$crate::test::Slot] = &[
                $( $crate::test::slot::<$ty>(::core::stringify!($param)) ),*
            ];
            const PARAMS: &[$crate::grant::Param] =
                &$crate::test::grants::<{ $crate::test::grant_count(SLOTS) }>(SLOTS);
            /// The test's manifest: what it takes, as data in the test binary.
            static MANIFEST: $crate::grant::Manifest = $crate::grant::Manifest::new(PARAMS);
            const SETUP_VALUE: $crate::test::Setup =
                $crate::__test_setup!(@ $crate::test::Setup::DEFAULT ; $($arg)*).checked(PARAMS);
            /// What its grants start with, its seed and its faults left out: data in the binary.
            static SETUP: $crate::test::Setup = SETUP_VALUE;
            const PLANNED: $crate::test::Planned =
                $crate::test::Planned::of(&SETUP_VALUE, PARAMS).drawing(&SETUP_VALUE, $crate::test::drawn(SLOTS));
            $crate::test::granted(
                $crate::test::Test {
                    package: ::core::env!("CARGO_PKG_NAME"),
                    module: ::core::module_path!(),
                    name: ::core::stringify!($name),
                    manifest: &MANIFEST,
                    setup: &SETUP,
                    planned: PLANNED,
                },
                $crate::__test_body!($cx : $cxty $(, $param : $ty)* ; $(-> $returns)? $body),
            )
        }
    };
}
