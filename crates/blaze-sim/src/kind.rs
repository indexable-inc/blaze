//! A test's kind (D165: "a test declares its kind, and the kind decides its tier").
//!
//! Five kinds: a unit test (one input, one answer), a simulation (a property or deterministic
//! simulation on the tape, blaze-sim's own cases), a world test (a whole in-memory World), a scale
//! test (a large input, run optimized), a process test (a real child process, a real disk).
//!
//! A test declares its kind with [`kind_test!`](crate::kind_test), which writes the test's body
//! as a plain function and a module of the same name holding the one `#[test]` function, named
//! for the kind: the test's path is `<module>::<name>::kind_<kind>` (`Kind::leaf`). The round
//! script tiers by that leaf, so tiers come from kinds and not from a hand list of name fragments:
//!
//! ```text
//! cargo test -p blaze-cas -- --list | grep '::kind_scale: test$'   # the scale tier
//! cargo test -p blaze-cas -- ::kind_simulation                     # the simulation tier
//! cargo test -p blaze-cas -- --skip ::kind_scale --skip ::kind_process
//! ```
//!
//! A kind the macro does not know does not compile. The kind's entry point inside the body is the
//! test's own: a simulation calls [`simulate`] (which enforces "never varied") or
//! [`controls`](crate::controls); its name is printed with each pass line, since the thread is
//! named after the test.

use crate::check::{Outcome, check};
use crate::observe::ObserveMode;
use crate::search::Config;
use crate::tape::Tape;

/// The kinds of test, each with the name of its `#[test]` function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Unit,
    Simulation,
    World,
    Scale,
    Process,
}

impl Kind {
    /// Every kind, in tier order: the cheapest first.
    pub const ALL: [Kind; 5] = [Kind::Unit, Kind::Simulation, Kind::World, Kind::Scale, Kind::Process];

    /// The name [`kind_test!`](crate::kind_test) gives the `#[test]` function of a test of this
    /// kind: the last segment of the test's path, what a runner filters on.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn leaf(self) -> &'static str {
        match self {
            Kind::Unit => "kind_unit",
            Kind::Simulation => "kind_simulation",
            Kind::World => "kind_world",
            Kind::Scale => "kind_scale",
            Kind::Process => "kind_process",
        }
    }

    /// The kind of the test named `test` (a libtest path), by its last segment; `None` for a test
    /// not declared with [`kind_test!`](crate::kind_test).
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn of_test(test: &str) -> Option<Kind> {
        let leaf = test.rsplit("::").next()?;
        Kind::ALL.into_iter().find(|kind| kind.leaf() == leaf)
    }
}

/// The entry point of a simulation: [`check`] under `ObserveMode::Enforce`, so a run whose
/// passing cases drew some label (a limit, a choice) as one value in every case fails, naming it
/// (D165). What `config` says of observing is replaced.
#[track_caller]
pub fn simulate<O: Outcome>(config: Config, body: impl FnMut(&Tape) -> O) {
    check(config.observe(ObserveMode::Enforce), body);
}

/// Declares a test of a kind: `unit`, `simulation`, `world`, `scale` or `process`.
///
/// ```ignore
/// blaze_sim::kind_test! {
///     /// The log holds at most its drawn cap, and loses nothing it took.
///     simulation fn the_log_holds_its_cap() {
///         blaze_sim::simulate(Config::cases(256).must_reach_points(LOG_REACH), |tape| log_run(tape, NoPlant));
///     }
/// }
/// ```
///
/// writes `fn the_log_holds_its_cap() { .. }` where it stands (so the body resolves as any code
/// there does) and `mod the_log_holds_its_cap { #[test] fn kind_simulation() { super::the_log_holds_its_cap() } }`,
/// so the test's path ends in `::the_log_holds_its_cap::kind_simulation`. Attributes before the
/// kind go on the body's function (docs); `#[ignore]` is not taken: a test that cannot run yet is
/// not declared.
///
/// Each arm writes its whole expansion: the macro names no other macro of this crate, since a
/// `$crate::` path to one is refused inside the crate (lib.rs, at `#[macro_use]`).
#[macro_export]
macro_rules! kind_test {
    ($(#[$meta:meta])* unit fn $name:ident() $body:block) => {
        $(#[$meta])*
        fn $name() $body

        mod $name {
            #[test]
            fn kind_unit() {
                super::$name()
            }
        }
    };
    ($(#[$meta:meta])* simulation fn $name:ident() $body:block) => {
        $(#[$meta])*
        fn $name() $body

        mod $name {
            #[test]
            fn kind_simulation() {
                super::$name()
            }
        }
    };
    ($(#[$meta:meta])* world fn $name:ident() $body:block) => {
        $(#[$meta])*
        fn $name() $body

        mod $name {
            #[test]
            fn kind_world() {
                super::$name()
            }
        }
    };
    ($(#[$meta:meta])* scale fn $name:ident() $body:block) => {
        $(#[$meta])*
        fn $name() $body

        mod $name {
            #[test]
            fn kind_scale() {
                super::$name()
            }
        }
    };
    ($(#[$meta:meta])* process fn $name:ident() $body:block) => {
        $(#[$meta])*
        fn $name() $body

        mod $name {
            #[test]
            fn kind_process() {
                super::$name()
            }
        }
    };
}
