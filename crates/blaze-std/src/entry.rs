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

use crate::{
    cx::Cx,
    io::{Io, Missing},
    mem::TASK_FRAME,
};

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

/// As [`run_in`], its root context on `pool` (the process's `Threads` layout, which
/// `crate::rt::run` starts): `main`'s tasks run on the pool's workers.
pub(crate) fn run_on<R: Outcome>(
    io: Io,
    pool: crate::task::threads::Pool,
    said: &mut dyn fmt::Write,
    main: impl FnOnce(&Cx, Io) -> R,
) -> Status {
    let frame = match blaze_mem::Frame::with_reserve(MAIN_SCRATCH, blaze_mem::Tag::Frames) {
        Ok(frame) => frame,
        Err(_) => {
            let stopped = crate::Stopped::OutOfMemory { arena: "main's frame", asked: MAIN_SCRATCH };
            let _ = write!(said, "bstd::entry: the root context of `main` could not be made: {stopped}");
            return Status::FAILED;
        }
    };
    let deadline = blaze_core::wait::Deadline::after(io.now(), MAIN_BUDGET);
    let cx = Cx::root_on(deadline, frame, io.clone(), pool);
    main(&cx, io).status(said)
}

/// A parameter `main` may take: the root context, the world, or a capability the world hands
/// out by type. `#[bstd::main]` ([`crate::main!`]) gives each of `main`'s parameters
/// `<Type as Root>::root(cx, &io)`, so which roots exist is this trait's impls, never a list
/// in a macro or the compiler; a capability the host does not offer stops the program before
/// `main` with its [`Missing`]. `Out` is not one: standard output and standard error are both
/// `Out`, so `main` takes `io` and asks [`Io::need_out`] or [`Io::need_err`].
#[diagnostic::on_unimplemented(
    message = "`main` cannot take a `{Self}`",
    label = "not a root of `main`",
    note = "the roots: `&Cx`, `Io`, `Fs`, `Clock`, `Rng`, `Env`, `Args`, `Host`, `Mem`, `Net`, \
            `Http`, `Streams`, `Term`, `Keys`, `Roots`, `Locale`, `User`, `Code`, `Window`, \
            `Automation`, `Opener`, `Power`, `Audio`, `Camera`, `Screen`, `Location`, \
            `Bluetooth`, `Usb`, `Serial`, `Clipboard`, `Notify` (bstd::entry::Root)"
)]
pub trait Root<'cx>: Sized {
    /// The root out of the context `main` is run in and its world.
    fn root(cx: &'cx Cx, io: &Io) -> Result<Self, Missing>;
}

impl<'cx> Root<'cx> for &'cx Cx {
    fn root(cx: &'cx Cx, _io: &Io) -> Result<&'cx Cx, Missing> {
        Ok(cx)
    }
}

impl Root<'_> for Io {
    fn root(_cx: &Cx, io: &Io) -> Result<Io, Missing> {
        // clone: the world's handle, one for `main`, as `run_in` hands one.
        Ok(io.clone())
    }
}

macro_rules! roots {
    ($($ty:path => $need:ident),* $(,)?) => {$(
        impl Root<'_> for $ty {
            fn root(_cx: &Cx, io: &Io) -> Result<$ty, Missing> {
                io.$need()
            }
        }
    )*};
}

roots! {
    crate::fs::Fs => need_fs,
    crate::clock::Clock => need_clock,
    crate::rng::Rng => need_rng,
    crate::env::Env => need_env,
    crate::args::Args => need_args,
    crate::host::Host => need_host,
    crate::mem::Mem => need_mem,
    crate::net::Net => need_net,
    crate::requests::Http => need_http,
    crate::streams::Streams => need_streams,
    crate::term::Term => need_term,
    crate::keys::Keys => need_keys,
    crate::roots::Roots => need_roots,
    crate::locale::Locale => need_locale,
    crate::user::User => need_user,
    crate::code::Code => need_code,
    crate::window::Window => need_windows,
    crate::automation::Automation => need_automation,
    crate::opener::Opener => need_opener,
    crate::power::Power => need_power,
    crate::audio::Audio => need_audio,
    crate::device::Camera => need_camera,
    crate::device::Screen => need_screen,
    crate::device::Location => need_location,
    crate::device::Bluetooth => need_bluetooth,
    crate::device::Usb => need_usb,
    crate::device::Serial => need_serial,
    crate::device::Clipboard => need_clipboard,
    crate::device::Notify => need_notify,
}

/// `#[bstd::main]`: a `main` that takes its world, one spelling for rustc and for blaze. It
/// keeps the function as written and makes the process's `main` the call [`run`] of a closure
/// that hands it each root by its type ([`Root`]):
///
/// ```ignore
/// #[bstd::main]
/// fn main(cx: &Cx, io: Io, fs: Fs) -> Result<(), bstd::entry::Error> { .. }
/// ```
///
/// A parameter is a name or `_`, then its type; a type that is not a root is the error of
/// [`Root`], which lists them. What `main` returns is an [`Outcome`]. It is a `macro_rules!`
/// attribute (`macro_attr`), as `#[bstd::test]` is, so it needs no proc macro and nothing
/// to run on `wasm32-wasip1` under blaze. A crate of the blaze edition needs none of it: its
/// front end appends the same call (this module's doc).
#[macro_export]
macro_rules! main {
    attr() (
        $(#[$meta:meta])*
        $vis:vis fn main($($root:tt : $ty:ty),* $(,)?) $(-> $returns:ty)? $body:block
    ) => {
        $(#[$meta])*
        $vis fn main() -> $crate::entry::Exit {
            fn main($($root: $ty),*) $(-> $returns)? $body
            $crate::entry::run(
                |_bstd_cx: &$crate::Cx, _bstd_io: $crate::Io| -> ::core::result::Result<_, $crate::Missing> {
                    ::core::result::Result::Ok(main($(
                        <$ty as $crate::entry::Root<'_>>::root(_bstd_cx, &_bstd_io)?
                    ),*))
                },
            )
        }
    };
}
