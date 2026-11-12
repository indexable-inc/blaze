//! The program's entry (docs/v2/plan/design/std.md, "The blaze edition: opting in"): what
//! stands between the process's start and a `main` that takes its world as parameters.
//!
//! A crate of the blaze edition writes `fn main(cx: &Cx, io: Io) -> Result<(), E>` (or a
//! shorter form the entry rule allows), and blaze's front end appends the one call that runs
//! it: [`run`] of a closure that hands `main` the roots it asked for and gives what it
//! returned to [`Outcome::status`]. Nothing here is a compiler special case: the call is
//! ordinary code, and a program not of the edition can write it itself.
//!
//! - [`run`] is the process's: it makes the native [`Io`] from what the process was started
//!   with, and is the one place that happens.
//! - [`run_in`] is the same on an `Io` its caller made: a test hands it [`Io::sim`], so a
//!   program's `main` runs in the simulation with no process (std.md, "Io").
//! - [`Outcome`] is the rule for what `main` may return, as a trait: `()`, a [`Status`], or a
//!   `Result` of one of those and an error that can be shown. [`Error`] is the error a `main`
//!   that uses `?` on more than one area's errors returns.
//!
//! The root context's bounds are bstd's to pick, since `main` has no caller: they are the
//! named constants [`MAIN_BUDGET`] and [`MAIN_SCRATCH`] (std.md, "The second rule": a bound
//! bstd must pick is a named `pub const`). A program that wants others takes `io` alone and
//! makes its own context with [`Io::cx`].

use alloc::{
    borrow::ToOwned as _,
    string::{String, ToString as _},
};
use core::{fmt, time::Duration};

use crate::{cx::Cx, io::Io, mem::TASK_FRAME};

pub use crate::rt::{Exit, run};

/// How long the root context of [`run`] lasts: for ever. A program's own deadline is a child
/// context ([`Cx::until`]), which only narrows.
pub const MAIN_BUDGET: Duration = Duration::MAX;

/// The most the root context's frame hands out: [`TASK_FRAME`], as a task's.
pub const MAIN_SCRATCH: usize = TASK_FRAME;

/// The status a program's process exits with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Status(pub u8);

impl Status {
    /// `main` did what it was for.
    pub const OK: Status = Status(0);
    /// `main` returned an error, or its world could not be made.
    pub const FAILED: Status = Status(1);
}

/// What a `main` of the blaze edition may return: anything that says how the program ended.
/// The rule is this trait, never a list in the compiler: a type of a program's own may
/// implement it.
pub trait Outcome {
    /// The status the process exits with; what a failure has to say is written to `said`,
    /// which the entry puts on standard error.
    fn status(self, said: &mut dyn fmt::Write) -> Status;
}

impl Outcome for () {
    fn status(self, _said: &mut dyn fmt::Write) -> Status {
        Status::OK
    }
}

impl Outcome for Status {
    fn status(self, _said: &mut dyn fmt::Write) -> Status {
        self
    }
}

/// `Ok` ends as its value does; `Err` says the error, as its `Display` writes it, and ends
/// [`Status::FAILED`].
impl<T: Outcome, E: fmt::Display> Outcome for Result<T, E> {
    fn status(self, said: &mut dyn fmt::Write) -> Status {
        match self {
            Ok(value) => value.status(said),
            Err(error) => {
                // A `said` that cannot take the message changes nothing of the status.
                let _ = write!(said, "{error}");
                Status::FAILED
            }
        }
    }
}

/// What a `main` returns when it failed: what the failure says, from whichever error it was.
/// `?` in `main` turns any error into it (`From` for every `core::error::Error`), so one
/// `main` can use capabilities whose errors differ; the entry prints it and exits
/// [`Status::FAILED`]. It is for `main` alone: below it a function returns its area's own
/// error (std.md, "Errors"), and it is not itself a `core::error::Error`, which is what lets
/// every error convert into it. Error unions (edition.md, change 2) replace it where a
/// `main` wants to say which errors it has.
pub struct Error {
    said: String,
}

impl Error {
    /// A failure that says `said`.
    pub fn new(said: &str) -> Error {
        // alloc: an error's message.
        Error { said: said.to_owned() }
    }
}

impl<E: core::error::Error> From<E> for Error {
    fn from(error: E) -> Error {
        // alloc: an error's message.
        Error { said: error.to_string() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.said)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.said)
    }
}

/// `main` run in the world `io`: its root context made with [`MAIN_BUDGET`] and
/// [`MAIN_SCRATCH`], `main` handed it and `io`, and what it returned turned into a status,
/// its failure's words written to `said`. A root frame that cannot be made is a failure that
/// says so, and `main` does not run.
pub fn run_in<R: Outcome>(io: Io, said: &mut dyn fmt::Write, main: impl FnOnce(&Cx, Io) -> R) -> Status {
    let cx = match io.cx(MAIN_BUDGET, MAIN_SCRATCH) {
        Ok(cx) => cx,
        Err(stopped) => {
            let _ = write!(said, "bstd::entry: the root context of `main` could not be made: {stopped}");
            return Status::FAILED;
        }
    };
    main(&cx, io).status(said)
}
