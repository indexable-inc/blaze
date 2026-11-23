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

use core::{fmt, time::Duration};

use blaze_own::FixedString;

use crate::{
    cx::Cx,
    io::Io,
    mem::TASK_FRAME,
};

pub use crate::rt::{Exit, StartBlock, run, run_with, run_with_heap, run_with_heap_at, start_block};

/// The process's heap, as the grant a `main` or a test is handed ([`crate::mem::Heap`]): the
/// ONE call of `blaze_own::Heap::process()` in the tree (D120). Every entry of bstd takes its
/// heap here and hands it down: the native start (`rt::launch_with`, `rt::run_with_heap`), the
/// hosted start (`guest::start`) and the test entries ([`launch_in`], `simtest::granted`). No
/// function below an entry reaches for a heap; it is handed one.
pub(crate) fn process_heap() -> crate::mem::Heap {
    crate::mem::Heap::of(blaze_own::Heap::process())
}

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
///
/// Its words are kept inline, at most [`ERROR_BYTES`] of them: `?` converts with no allocator
/// in reach, and the blaze edition's prelude reaches no global heap (D120). Words past the
/// bound are cut at a character's boundary, and the shown error says it was cut and how long
/// it was.
pub struct Error {
    said: FixedString<ERROR_BYTES>,
}

/// The most bytes of its words an [`Error`] keeps: a bound bstd picks (std.md, "The second
/// rule"), long enough for an error's message and its path.
pub const ERROR_BYTES: usize = 512;

/// `piece` added to `kept`: as much of it as fits, cut at a character's boundary; the rest is
/// refused, which marks `kept` cut and counts the bytes it wanted.
fn keep(kept: &mut FixedString<ERROR_BYTES>, piece: &str) {
    let mut end = piece.len().min(ERROR_BYTES - kept.len());
    while !piece.is_char_boundary(end) {
        end -= 1;
    }
    let (fits, cut) = piece.split_at(end);
    // `fits` is at most the room left, so it is taken unless an earlier piece was cut (then it
    // is refused and counted, as the cut is).
    let _ = kept.try_push_str(fits);
    if !cut.is_empty() {
        let _ = kept.try_push_str(cut);
    }
}

/// A `fmt::Write` over an error's inline words that cuts rather than fails.
struct Keep<'k>(&'k mut FixedString<ERROR_BYTES>);

impl fmt::Write for Keep<'_> {
    fn write_str(&mut self, piece: &str) -> fmt::Result {
        keep(self.0, piece);
        Ok(())
    }
}

impl Error {
    /// A failure that says `said`.
    pub fn new(said: &str) -> Error {
        let mut kept = FixedString::new();
        keep(&mut kept, said);
        Error { said: kept }
    }
}

impl<E: core::error::Error> From<E> for Error {
    fn from(error: E) -> Error {
        let mut kept = FixedString::new();
        // `Keep` never fails; an error's `Display` that fails leaves what it wrote.
        let _ = fmt::Write::write_fmt(&mut Keep(&mut kept), format_args!("{error}"));
        Error { said: kept }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.said.as_str())?;
        if self.said.is_refused() {
            write!(f, " [cut: the error said {} bytes, and an `Error` keeps {ERROR_BYTES}]", self.said.needed())?;
        }
        Ok(())
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
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
    let kept = Unflushed { io: io.clone(), armed: true };
    let status = main(&cx, io).status(said);
    kept.end(status, said)
}

/// The world `main` was handed, kept by the entry so that what its streams' buffers hold is
/// written when `main` ends ([`crate::out`], "Buffering"): by [`Unflushed::end`] when it
/// returns, by the drop when it unwinds (a panic, or a simulated [`crate::host::Host::exit`]).
struct Unflushed {
    io: Io,
    armed: bool,
}

impl Unflushed {
    /// `main` returned with `status`: the buffers written. A write refused turns an `OK` into
    /// `FAILED`, and what was refused is said after whatever `main`'s failure said.
    fn end(mut self, status: Status, said: &mut dyn fmt::Write) -> Status {
        self.armed = false;
        match self.io.flush_streams("main") {
            Ok(()) => status,
            Err(error) => {
                let _ = match status {
                    Status::OK => write!(said, "{error}"),
                    _ => write!(said, "; {error}"),
                };
                match status {
                    Status::OK => Status::FAILED,
                    failed => failed,
                }
            }
        }
    }
}

impl Drop for Unflushed {
    fn drop(&mut self) {
        if self.armed {
            // Unwinding already: the panic is what the run says; a write refused now has no
            // status left to change.
            let _ = self.io.flush_streams("main");
        }
    }
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
    let kept = Unflushed { io: io.clone(), armed: true };
    let status = main(&cx, io).status(said);
    kept.end(status, said)
}

/// The process's `main` of a `#[bstd::main]` program (the run spec read from file descriptor 3).
pub use crate::rt::{SPEC_MARK, SPEC_MARK_VALUE, launch, launch_with};
/// The root a program's entry takes, once, in its `fn main`, and hands down to
/// [`launch_with`] / [`run_with`]: the authority to touch the real OS (D102;
/// port27/bstd-capabilities.md P3). Named here so the `main!` macro's expansion reaches it as
/// `$crate::entry::Os` in a crate that depends on bstd alone. The process's heap is not
/// named here: bstd's entries take it themselves ([`process_heap`]) and hand it to `main` as a
/// grant (D120).
pub use blaze_core::Os;

/// [`launch`] on a world its caller made: what file descriptor 3 would hold is
/// `delivered` (`None`: closed), the process's world is `io`, and a manifest asked for is
/// written to `io`'s standard output. A test runs a `#[bstd::main]`-shaped program with it on
/// [`Io::sim`]: the macro's `body` is [`launch_body!`](crate::launch_body). It is a test's
/// entry, so it takes the process's heap ([`process_heap`], D120): the launcher's spec and
/// problems are made in it, and a `heap: Heap` parameter is handed it.
pub fn launch_in<R: Outcome>(
    manifest: &'static crate::grant::Manifest,
    delivered: Option<&[u8]>,
    io: Io,
    said: &mut dyn fmt::Write,
    body: impl FnOnce(&mut crate::grant::Given<'_>) -> Result<R, crate::grant::Refused>,
) -> Status {
    let heap = process_heap();
    let alloc: crate::grant::LaunchAlloc = heap.shared();
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
            run_in(io, said, crate::grant::main_of(manifest, &spec, started, fake.as_ref(), heap, body))
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
            const PARAMS: &[$crate::grant::Param] = &[
                $( $crate::grant::Param::of::<$ty>(stringify!($name)) ),*
            ];
            const DEFAULTS: &[$crate::grant::Spelling] = &$crate::__grant_spellings!($($default)*);
            static MANIFEST: $crate::grant::Manifest = $crate::grant::Manifest::new(PARAMS)
                // The attribute's defaults (grant/defaults.rs), checked here as the crate compiles.
                .with_defaults(DEFAULTS);
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
