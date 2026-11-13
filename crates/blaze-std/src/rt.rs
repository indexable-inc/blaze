//! What bstd still takes from Rust's `std`, all of it, in one module.
//!
//! bstd is `#![no_std]`: every other module names only `core`, `alloc` and blaze's crates. The
//! few things Rust's `std` alone provides today are named here and nowhere else, and this
//! module is a row of S4's ban list (`harness/v2/nostd.json`): it shrinks as `bstd::rt` (the
//! process's entry, unwinding and exit on `blaze-sys`) is built, and is deleted with the row
//! (docs/v2/plan/design/sys.md, "Runtime" and "Migration").
//!
//! | here | used by | replaced by |
//! |---|---|---|
//! | [`RootPath`] | `NativeWorld::root`, which blaze-core's drivers take as a `std::path::Path` | a `Dir` handle from the start block, when the drivers open beneath a handle |
//! | [`process_id`] | `Host`'s pid on a native `Io` | `blaze_sys`'s `getpid` |
//! | [`exit`] | `Host::exit` on a native `Io` | `blaze_sys`'s `exit_group` |
//! | [`catch_unwind`], [`resume_unwind`] | a task's panic failing only its task; the simulated exit's unwind | bstd's own unwinder entry (on `library/unwind`) |
//! | [`LazyLock`], [`OnceLock`] | `bstd::sync`'s `Lazy` and `Once` | the same two on bstd's one wait |
//! | [`run`], [`Exit`] | the entry of a program of the blaze edition (`bstd::entry`): the native world made from the process's arguments, environment and start directory, and the status handed to Rust std's `lang_start` | bstd's own entry on `blaze-sys`: the start block read directly, no `lang_start` |
//! | [`write_out`] | `Out` on a native `Io`, both streams | an effect of blaze-core's drivers that writes to a handle the process was started with |
//! | `Owns` for `HashMap`, `HashSet`, `Mutex`, `RwLock`, `OnceLock`, `LazyLock` | `bstd::own`'s reachability walk, for code that still holds one of Rust std's | nothing: gone with the last use of those types |

extern crate std;

pub(crate) use std::{
    panic::{catch_unwind, resume_unwind},
    path::Path as RootPath,
};

use alloc::{string::{String, ToString as _}, sync::Arc, vec::Vec};
use core::fmt::Write as _;

use crate::{
    cx::Cx,
    entry::{Outcome, Status, run_in},
    io::{Io, Limits, NativeWorld},
};

/// The threads the native driver runs file calls on where it has a pool (kqueue, IOCP): a
/// bound bstd picks for a program's entry, named (std.md, "The second rule").
pub(crate) const FILE_THREADS: usize = 4;

/// What a program of the blaze edition hands Rust std's `lang_start`: its [`Status`], as the
/// one type of bstd that implements Rust std's `Termination`. Gone with `lang_start`, when
/// bstd's own entry reads the start block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Exit(pub Status);

impl std::process::Termination for Exit {
    fn report(self) -> std::process::ExitCode {
        std::process::ExitCode::from(self.0.0)
    }
}

/// The process's entry for a `main` that takes its world: the native [`Io`] over the
/// directory the process was started in, with its command line, its environment and
/// [`Limits::USUAL`]; then [`run_in`]. What a failure says goes to standard error as one line,
/// `error: <what>`, and the process exits with the status (0, or 1 for a failure).
///
/// What the world is made from, each read here once and nowhere else:
/// - the command line: an argument that is not UTF-8 stops the program before `main`, naming
///   its place (`Args` holds text);
/// - the environment: a variable whose name or value is not UTF-8 is not in `Env`, which
///   holds text (a stated gap until `Env` holds bytes);
/// - the start directory, the root every `Path` is beneath;
/// - the seed of `Rng`, from the system's randomness as Rust std's hasher keys draw it;
/// - the host's name, empty: neither blaze-core nor Rust std asks the system for it yet.
pub fn run<R: Outcome>(main: impl FnOnce(&Cx, Io) -> R) -> Exit {
    // alloc: once per process, what a failure says.
    let mut said = String::new();
    let status = match native() {
        Ok(io) => run_in(io, &mut said, main),
        Err(why) => {
            said.push_str(&why);
            Status::FAILED
        }
    };
    if !said.is_empty() {
        error_line(&said);
    }
    Exit(status)
}

/// The process's world, or why it cannot be made.
fn native() -> Result<Io, String> {
    // alloc: once per process, its command line.
    let mut args = Vec::new();
    for (at, arg) in std::env::args_os().enumerate() {
        match arg.into_string() {
            Ok(arg) => args.push(arg),
            Err(arg) => {
                // alloc: an error's message.
                let mut why = String::new();
                let _ = write!(
                    why,
                    "bstd::entry: argument {at} of the command line is not UTF-8 ({}): `Args` holds text",
                    arg.to_string_lossy()
                );
                return Err(why);
            }
        }
    }
    // alloc: once per process, its environment.
    let env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    let root = std::env::current_dir().map_err(|error| {
        // alloc: an error's message.
        let mut why = String::new();
        let _ = write!(why, "bstd::entry: the directory the program was started in: {error}");
        why
    })?;
    let seed = {
        use std::hash::BuildHasher as _;
        std::hash::RandomState::new().hash_one(0u8)
    };
    let world = NativeWorld {
        root: &root,
        threads: FILE_THREADS,
        env,
        args,
        // alloc: once per process, the host's name (empty: see `run`).
        host_name: Arc::from(""),
        seed,
        limits: Limits::USUAL,
    };
    // alloc: an error's message.
    Io::native(world).map_err(|error| error.to_string())
}

/// `said` on standard error, as `error: <said>` and a line feed. A standard error that
/// cannot be written to changes nothing: the status still says the program failed.
fn error_line(said: &str) {
    use std::io::Write as _;
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(b"error: ").and_then(|()| err.write_all(said.as_bytes())).and_then(|()| err.write_all(b"\n"));
}

/// `parts`, in order, on the process's `stream`, with the operating system when this
/// returns: all of them or the host's refusal as it says it. Standard output goes through
/// Rust std's handle, whose buffer is flushed here, so `parts` reach the system as one write
/// when they fit it and never wait in it; standard error has no buffer.
pub(crate) fn write_out(stream: crate::out::Stream, parts: &[&[u8]]) -> Result<(), String> {
    use std::io::Write as _;
    fn all(mut to: impl std::io::Write, parts: &[&[u8]]) -> std::io::Result<()> {
        for part in parts {
            to.write_all(part)?;
        }
        to.flush()
    }
    let written = match stream {
        crate::out::Stream::Out => all(std::io::stdout().lock(), parts),
        crate::out::Stream::Err => all(std::io::stderr().lock(), parts),
    };
    // alloc: an error's message.
    written.map_err(|error| error.to_string())
}

/// The process's id.
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub(crate) fn process_id() -> u32 {
    std::process::id()
}

/// Ends the process with `status`.
#[blaze::pure]
pub(crate) fn exit(status: i32) -> ! {
    std::process::exit(status)
}

// How the containers only Rust's `std` has hold their type arguments, for `bstd::own`'s walk
// (`own.rs` states core's and alloc's, with the rule): the maps behind a pointer they alone
// own, the locks and the once-cells in place, changeable through a shared reference.
#[expect(
    clippy::disallowed_types,
    reason = "how each of std's containers holds its contents is stated once, for the names \
              bstd replaces too: code that still holds one is walked the same way"
)]
#[expect(unsafe_code, reason = "the trusted `Owns` assertions for std's own containers, with their SAFETY")]
mod owns {
    // `super::std`: bstd is `#![no_std]`, so `std` is a name only where `rt` declares it.
    use super::std::{
        collections::{HashMap, HashSet},
        sync::{LazyLock, Mutex, OnceLock, RwLock},
    };

    use crate::own::{Mutably, Owns, Solely};

    // As `own.rs` says of the others: each keeps a value of each type argument alive the way
    // its impl says, and keeps alive nothing else that owns.

    // SAFETY: a `HashMap` keeps each key and value in a table behind a pointer it alone
    // owns, and its hasher `S` in place (walked as solely owned too, which only refuses more).
    unsafe impl<K, V, S> Owns<Solely> for HashMap<K, V, S> {}
    // SAFETY: a `HashSet` is a `HashMap<T, (), S>`: its values behind a pointer it alone owns.
    unsafe impl<T, S> Owns<Solely> for HashSet<T, S> {}
    // SAFETY: a `Mutex` holds its value in place in an `UnsafeCell`; `lock` through a shared
    // reference gives `&mut T`, so the value is changeable after it exists.
    unsafe impl<T: ?Sized> Owns<Mutably> for Mutex<T> {}
    // SAFETY: as `Mutex`: the value in place in an `UnsafeCell`, `write` through a shared
    // reference gives `&mut T`.
    unsafe impl<T: ?Sized> Owns<Mutably> for RwLock<T> {}
    // SAFETY: a `OnceLock` holds its value in place and `set` writes it through a shared
    // reference, once, after the cell exists.
    unsafe impl<T> Owns<Mutably> for OnceLock<T> {}
    // SAFETY: a `LazyLock` holds its initializer `F` and then its value `T` in place, the
    // one replaced by the other through a shared reference on first use.
    unsafe impl<T, F> Owns<Mutably> for LazyLock<T, F> {}
}
