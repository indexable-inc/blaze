//! The program's entry (docs/v2/plan/design/std.md, "The blaze edition: opting in"): what
//! stands between the process's start and a `main` that takes its world as parameters.
//!
//! A crate of the blaze edition writes `fn main(cx: &Cx, out: Out) -> Result<(), E>`: after the
//! context, each parameter is a grant it names (D104: `out: Out`, `args: Args`, `clock: Clock`,
//! `data: Dir<ReadOnly>`, ...; a whole `Io` cannot be asked for). The root's `fn main` is
//! expanded by [`main!`](crate::main) with no attribute written, into [`run`] of a closure that
//! makes each grant and hands it over, and gives what `main` returned to [`Outcome::status`].
//! Nothing here is a compiler special case: the expansion is ordinary code, and a program not
//! of the edition writes `#[bstd::main]` for the same.
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
//! bstd must pick is a named `pub const`). A program that wants narrower ones makes a child
//! context ([`Cx::until`]).

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

pub use crate::rt::{Exit, run, run_with};

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
    /// `main` was not started: a grant it takes is missing or wrong, or the run spec was
    /// refused (crate::grant). 77 is `EX_NOPERM` of sysexits.h: permission denied.
    pub const REFUSED: Status = Status(77);
}

/// What a `main` of the blaze edition may return: anything that says how the program ended.
/// The rule is this trait, never a list in the compiler: a type of a program's own may
/// implement it.
pub trait Outcome {
    /// The status the process exits with; what a failure has to say is written to `said`,
    /// which the entry puts on standard error.
    fn status<W: fmt::Write + ?Sized>(self, said: &mut W) -> Status;
}

impl Outcome for () {
    fn status<W: fmt::Write + ?Sized>(self, _said: &mut W) -> Status {
        Status::OK
    }
}

impl Outcome for Status {
    fn status<W: fmt::Write + ?Sized>(self, _said: &mut W) -> Status {
        self
    }
}

/// `Ok` ends as its value does; `Err` says the error, as its `Display` writes it, and ends
/// [`Status::FAILED`].
impl<T: Outcome, E: fmt::Display> Outcome for Result<T, E> {
    fn status<W: fmt::Write + ?Sized>(self, said: &mut W) -> Status {
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

/// The process's `main` of a `#[bstd::main]` program (the run spec read from file descriptor 3).
pub use crate::rt::{SPEC_MARK, SPEC_MARK_VALUE, launch, launch_with};
/// The two roots a program's entry takes, once, in its `fn main`, and hands down to
/// [`launch_with`] / [`run_with`]: the authority to touch the real OS and the process's heap
/// (D102; port27/bstd-capabilities.md P3). Named here so the `main!` macro's expansion reaches
/// them as `$crate::entry::Os` and `$crate::entry::Heap` in a crate that depends on bstd alone.
pub use blaze_core::Os;
pub use blaze_own::Heap;

/// [`launch`] on a world its caller made: what file descriptor 3 would hold is
/// `delivered` (`None`: closed), the process's world is `io`, and a manifest asked for is
/// written to `io`'s standard output. A test runs a `#[bstd::main]`-shaped program with it on
/// [`Io::sim`]: the macro's `body` is [`launch_body!`](crate::launch_body). The launcher's
/// spec and problems are made in `alloc`, which the caller (the test, its own entry) hands in
/// (D102): `blaze_own::testing::heap().shared()`.
pub fn launch_in<R: Outcome>(
    manifest: &'static crate::grant::Manifest,
    delivered: Option<&[u8]>,
    io: Io,
    said: &mut dyn fmt::Write,
    alloc: crate::grant::LaunchAlloc,
    body: impl FnOnce(&mut crate::grant::Given<'_>) -> Result<R, crate::grant::Refused>,
) -> Status {
    let defaults = || {
        let (args, env) = (blaze_own::Vec::new_in(alloc), blaze_own::Vec::new_in(alloc));
        crate::spec::RunSpec::defaults(args, env, blaze_own::String::from_str_in("/", alloc), alloc)
    };
    match crate::grant::prepare(manifest, delivered, defaults, &[], alloc) {
        crate::grant::Prepared::Manifest(text) => match io.write_out(crate::out::Stream::Out, &[text.as_bytes(), b"\n"]) {
            Ok(()) => Status::OK,
            Err(why) => {
                let _ = write!(said, "{why}");
                Status::FAILED
            }
        },
        crate::grant::Prepared::Refuse(text) => {
            let _ = write!(said, "{}", text.as_str());
            Status::REFUSED
        }
        crate::grant::Prepared::Run { spec, started, fake, taken: _ } => {
            run_in(io, said, crate::grant::main_of(manifest, &spec, started, fake.as_ref(), body))
        }
    }
}

/// `#[bstd::main]`: a `main` that says what it takes in its signature, one spelling for rustc
/// and for blaze (docs/v2/plan/design/run.md, "What a program is given"). After `cx: &Cx`,
/// each parameter is a grant: its type the kind ([`crate::grant::Grant`]) and its name the
/// grant's name:
///
/// ```ignore
/// #[bstd::main]
/// fn main(cx: &Cx, out: Out, data: Dir<ReadOnly>) -> Result<(), bstd::entry::Error> { .. }
/// ```
///
/// In a blaze edition no attribute is written: the binary root's `fn main` is this macro's
/// input unasked (D104; blaze-scope `IMPLICIT_MAIN`). A `main` that takes nothing is taken too,
/// as one whose manifest is empty.
///
/// It keeps the function as written, puts the program's manifest in the binary as a `static`
/// ([`crate::grant::Manifest`]), and makes the process's `main` the launcher
/// ([`crate::rt::launch`]): every parameter made from the run spec before `main` runs, or every
/// problem printed with its fix and [`Status::REFUSED`]. A type that is not a `Grant` is a
/// compile error at its parameter; an `Out` not named `out` or `err` is one too. What `main`
/// returns is an [`Outcome`]. It is a `macro_rules!` attribute (`macro_attr`), as
/// `#[bstd::test]` is, so it needs no proc macro.
#[macro_export]
macro_rules! main {
    // A `main` that takes nothing (D104): no context and no grant. It is wrapped as one that
    // takes `cx` and nothing else, so it starts, is hosted and is launched as every `main` is,
    // with an empty manifest.
    attr() (
        $(#[$meta:meta])*
        $vis:vis fn main() $(-> $returns:ty)? $body:block
    ) => {
        $(#[$meta])*
        $vis fn main() -> $crate::entry::Exit {
            fn main() $(-> $returns)? $body
            fn rooted(_cx: &$crate::Cx) $(-> $returns)? {
                main()
            }
            /// The program's manifest: it takes nothing.
            static MANIFEST: $crate::grant::Manifest = $crate::grant::Manifest::new(&[]);
            /// The start of the program hosted by `blaze run` (run.md, "The hosted world").
            #[unsafe(no_mangle)]
            pub extern "C" fn bstd_hosted_start(table: *const $crate::wire::HostTable) -> i32 {
                fn hosted(cx: &$crate::Cx, io: $crate::Io) -> $crate::entry::Status {
                    let ran = rooted(cx);
                    $crate::guest::finish(&io, ran)
                }
                $crate::guest::start(table, hosted)
            }
            $crate::entry::launch_with(
                $crate::entry::Os::process(),
                $crate::entry::Heap::process(),
                &MANIFEST,
                $crate::launch_body!(rooted;),
            )
        }
    };
    attr($($default:tt)*) (
        $(#[$meta:meta])*
        $vis:vis fn main($cx:tt : $cxty:ty $(, $name:tt : $ty:ty)* $(,)?) $(-> $returns:ty)? $body:block
    ) => {
        $(#[$meta])*
        $vis fn main() -> $crate::entry::Exit {
            fn main($cx: $cxty $(, $name: $ty)*) $(-> $returns)? $body
            /// The program's manifest: what it takes, as data in the binary.
            static MANIFEST: $crate::grant::Manifest = $crate::grant::Manifest::new(&[
                $( $crate::grant::Param::of::<$ty>(stringify!($name)) ),*
            ])
            // The attribute's defaults (grant/defaults.rs), checked here as the crate compiles.
            .with_defaults(&$crate::__grant_spellings!($($default)*));
            /// The start of the program hosted by `blaze run` (run.md, "The hosted world"):
            /// each parameter asked of the host, then `main`, over the host's table.
            #[unsafe(no_mangle)]
            pub extern "C" fn bstd_hosted_start(table: *const $crate::wire::HostTable) -> i32 {
                fn hosted(cx: &$crate::Cx, io: $crate::Io) -> $crate::entry::Status {
                    let ran = main(cx $(, match $crate::guest::grant::<$ty>(&io, stringify!($name)) {
                        ::core::option::Option::Some(granted) => granted,
                        ::core::option::Option::None => return $crate::entry::Status::REFUSED,
                    })*);
                    $crate::guest::finish(&io, ran)
                }
                $crate::guest::start(table, hosted)
            }
            $crate::entry::launch_with(
                $crate::entry::Os::process(),
                $crate::entry::Heap::process(),
                &MANIFEST,
                $crate::launch_body!(main; $($name : $ty),*),
            )
        }
    };
}

/// The launcher's body for a `main` whose parameters after `cx` are `$name: $ty`: each checked
/// by making it, once, in written order (crate::grant's made.rs); when every one was made and
/// the spec grants nothing else, `main` is called with the values the checks made; else every
/// problem is the refusal, and what was made is dropped in order. For [`main!`] and for a test
/// that runs a program through [`launch_in`].
#[macro_export]
macro_rules! launch_body {
    ($main:ident; $($name:tt : $ty:ty),*) => {
        |given: &mut $crate::grant::Given<'_>| {
            let checking = given.checking();
            $( let checking = checking.check::<$ty>(stringify!($name)); )*
            let filled = checking.filled()?;
            let cx = filled.cx();
            let $crate::__made!($($name),*) = filled.hand_out();
            ::core::result::Result::Ok($main(cx $(, $name)*))
        }
    };
}
