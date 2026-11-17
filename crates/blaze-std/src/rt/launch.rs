//! The process's side of the launcher `#[bstd::main]` makes (crate::grant): the run spec read
//! from file descriptor 3, the native world made from it, and `main` run on it. Rust std is
//! named here as in the rest of `rt`: to open the descriptor, to read it with a deadline, and to
//! hand a path to the driver.
//!
//! **File descriptor 3** (docs/v2/plan/design/run.md, "What a program is given"), read only when
//! [`SPEC_MARK`] says the parent put the spec there; without it the start is ordinary, and only
//! what fills itself (and `Io`) is filled. Marked, it must hold `bstd spec\n` and a spec, or
//! `bstd manifest\n`, written by the parent and closed: anything else, or nothing within
//! [`SPEC_WAIT`], is a refusal naming it, never a start without it. It is opened as `/dev/fd/3`
//! (on Linux a link to `/proc/self/fd/3`), so no `unsafe` adopts a descriptor.

use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::time::Duration;

use blaze_own::SharedAlloc;

use super::std::{self, io::Read as _};
use crate::{
    entry::Status,
    grant::{Given, LaunchAlloc, Manifest, Prepared, Refused, main_of, prepare},
    out::Stream,
    spec::{RunSpec, SPEC_HEADER, SPEC_LIMITS},
};

use super::{Exit, error_line, open_native, process_start, run_io, start_dir, write_out};
use crate::{args::ArgList, env::Vars, grant::Started};

/// How long the launcher waits for the parent to finish writing the spec and close its end.
pub(crate) const SPEC_WAIT: Duration = Duration::from_secs(5);

/// Where file descriptor 3 is opened from.
const SPEC_FD: &str = "/dev/fd/3";

/// The process's `main` of a `#[bstd::main]` program: what file descriptor 3 asks, then
/// `main` with every parameter made, or the refusal and [`Status::REFUSED`].
///
/// It reaches the OS's root itself: [`launch_with`], to which the program's entry hands it, is
/// the form to call; this one goes in batch B1 (port27/bstd-capabilities.md).
pub fn launch<R: crate::entry::Outcome>(
    manifest: &'static Manifest,
    body: impl FnOnce(&mut Given<'_>) -> Result<R, Refused>,
) -> Exit {
    launch_with(blaze_core::Os::process(), manifest, body)
}

/// [`launch`] with the OS's root handed down by the program's entry, its `fn main`, where
/// `blaze_core::Os::process()` is taken (D102, plan port27/bstd-capabilities.md P3): `os`
/// makes the native world. The process's heap is taken here, by bstd's entry
/// (`entry::process_heap`, D120): every allocation the launcher makes is in it, and a
/// `heap: Heap` parameter of `main` is handed it.
pub fn launch_with<R: crate::entry::Outcome>(
    os: &'static blaze_core::Os,
    manifest: &'static Manifest,
    body: impl FnOnce(&mut Given<'_>) -> Result<R, Refused>,
) -> Exit {
    let delivered = match read_fd3() {
        Ok(delivered) => delivered,
        Err(why) => {
            error_line(&why);
            return Exit(Status::REFUSED);
        }
    };
    let block = match process_start() {
        Ok(block) => block,
        Err(why) => {
            error_line(&why);
            return Exit(Status::FAILED);
        }
    };
    // The process's heap, taken by bstd's entry, once, and handed down (D120).
    let heap = crate::entry::process_heap();
    let alloc: SharedAlloc<'static> = heap.shared();
    // An ordinary start's spec holds no command line, environment or directory: the world reads
    // the process's own, where the kernel left them (std.md, "What a program pays for at
    // start"), so nothing is copied into it, sorted, or copied back out.
    let defaults = || -> RunSpec<LaunchAlloc> {
        RunSpec::defaults(blaze_own::Vec::new_in(alloc), blaze_own::Vec::new_in(alloc), blaze_own::String::from_str_in(".", alloc), alloc)
    };
    match prepare(manifest, delivered.as_deref(), defaults, &block.args, alloc) {
        Prepared::Manifest(text) => match write_out(Stream::Out, &[text.as_bytes(), b"\n"]) {
            Ok(()) => Exit(Status::OK),
            Err(why) => {
                error_line(&why);
                Exit(Status::FAILED)
            }
        },
        Prepared::Refuse(said) => {
            error_line(said.as_str());
            Exit(Status::REFUSED)
        }
        Prepared::Run { spec, started, fake, taken } => {
            let world = match started {
                // A command reads every folder above the one it was started in; its command line
                // is the process's, less the `--grant`s the launcher took.
                Started::Ordinary => {
                    open_native(os, ArgList::process_after(block, taken), Vars::process(block), start_dir(), u8::MAX)
                }
                // A spec-made world is the spec's alone: its command line, environment and
                // directory (`RunSpec::read` refuses a directory that is not absolute).
                Started::WithSpec => {
                    let given = spec.get();
                    let args: Vec<String> = given.args.iter().map(|arg| arg.as_str().to_string()).collect();
                    let env: Vec<(String, String)> =
                        given.env.iter().map(|(name, value)| (name.as_str().to_string(), value.as_str().to_string())).collect();
                    // No folder above the spec's directory until a grant names levels.
                    open_native(os, ArgList::given(args), Vars::given(env), std::path::Path::new(given.dir.as_str()), 0)
                }
            };
            run_io(world, main_of(manifest, &spec, started, fake.as_ref(), heap, body))
        }
    }
}

/// The variable a parent sets to say file descriptor 3 holds the run spec: [`SPEC_MARK_VALUE`].
pub const SPEC_MARK: &str = "BSTD_RUN_SPEC";
/// Its one value: the descriptor.
pub const SPEC_MARK_VALUE: &str = "fd3";

/// What file descriptor 3 held, all of it; `None` when the parent did not say it holds a spec.
/// Only [`SPEC_MARK`] says so: a descriptor 3 inherited by accident (a build tool's jobserver
/// pipe, a shell's redirect) is never read, so it is neither stolen from nor waited on.
fn read_fd3() -> Result<Option<Vec<u8>>, String> {
    match std::env::var_os(SPEC_MARK) {
        None => return Ok(None),
        Some(mark) if mark == SPEC_MARK_VALUE => {}
        Some(mark) => {
            let mut why = String::new();
            let _ = core::fmt::Write::write_fmt(
                &mut why,
                format_args!("bstd: {SPEC_MARK} is `{}`, and its one value is `{SPEC_MARK_VALUE}`", mark.to_string_lossy()),
            );
            return Err(why);
        }
    }
    let file = match std::fs::File::open(SPEC_FD) {
        Ok(file) => file,
        Err(error) => {
            let mut why = String::new();
            let _ = core::fmt::Write::write_fmt(
                &mut why,
                format_args!("bstd: {SPEC_MARK} says file descriptor 3 holds the run spec, and it cannot be opened ({error})"),
            );
            return Err(why);
        }
    };
    let most = (SPEC_HEADER.len() + SPEC_LIMITS.most_bytes + 1) as u64;
    let (told, heard) = std::sync::mpsc::channel();
    // The read is on a thread of its own so its wait has a deadline (rule bounded-progress): a
    // parent that never closes its end leaves that thread blocked, and the process refuses.
    let reader = std::thread::Builder::new().name("bstd-spec".into()).spawn(move || {
        let mut bytes = Vec::new();
        let read = file.take(most).read_to_end(&mut bytes).map(|_| bytes);
        let _ = told.send(read);
    });
    if let Err(error) = reader {
        let mut why = String::new();
        let _ = core::fmt::Write::write_fmt(&mut why, format_args!("bstd: the thread that reads the run spec did not start: {error}"));
        return Err(why);
    }
    match heard.recv_timeout(SPEC_WAIT) {
        Ok(Ok(bytes)) => Ok(Some(bytes)),
        Ok(Err(error)) => {
            let mut why = String::new();
            let _ = core::fmt::Write::write_fmt(&mut why, format_args!("bstd: file descriptor 3 could not be read: {error}"));
            Err(why)
        }
        Err(_) => {
            let mut why = String::new();
            let _ = core::fmt::Write::write_fmt(
                &mut why,
                format_args!(
                    "bstd: file descriptor 3 is open and its writer did not close it within {}s: a run spec is written whole and the pipe closed",
                    SPEC_WAIT.as_secs()
                ),
            );
            Err(why)
        }
    }
}
