//! `blaze run` for a program that runs in its own process (run.md piece 6; bottom.md section
//! 11; decision D103): a Rust-edition program, whose start is the library's `lang_start::<T>`
//! called by the C entry shim (LADDER.md L1). It never runs inside blaze's process: its
//! `process::exit` would end blaze, and its `argv` would be blaze's own.
//!
//! The steps, each a failure before the program's first instruction that ends `blaze run` with
//! `program::NOT_RUN` (125, main.rs) and names the step ([`OwnError`]):
//!
//! 1. the program's code records laid out under the home's records folder
//!    ([`crate::run_parent::lay_out`] into `<HOME>/` [`crate::run_parent::RECORDS`]);
//! 2. the launcher found beside this binary ([`LAUNCHER`]);
//! 3. the launcher started with the program's `argv[0]` (the binary target's name), the words
//!    after `--`, the command's own environment plus the run mode's three variables
//!    ([`crate::run_parent::launch_env`]), the command's working directory, and the command's
//!    own terminal as its standard input, output and error;
//! 4. then the parent's loop ([`crate::run_loop::run_program`]), under D103's signal rules,
//!    until the program ends: its status is `blaze run`'s ([`crate::run_parent::status_of`]).
//!
//! THE ONE WAIT WITHOUT A DEADLINE (D103, rule 18's named exemption): the program's lifetime
//! is the user's (a server runs for days), so the loop's run has no deadline of its own
//! ([`crate::run_loop::Bounds::run`]); each of its waits is still one tick long, and every
//! step of the parent's own work keeps its bound (the start, a write of answers, the grace
//! after a forwarded SIGTERM). The span it runs in says so: [`PROGRAM_SPAN`].

use core::fmt;

use blaze_ladder::refusal::SourceFile;
use blaze_ladder::run::NewFront;
use blaze_own::Vec;
use bstd::Cx;
use bstd::env::VarName;
use bstd::io::Missing;
use bstd::mem::Frame;
use bstd::path::Path;

use crate::program::Launch;
use crate::run_loop::{self, LoopError};
use crate::run_parent::{self, ParentError, RECORDS};

/// The span the parent's loop runs in: the wait for the program's end, which D103 exempts from
/// a deadline (the module's doc says why).
pub(crate) const PROGRAM_SPAN: &str = "blaze run: the program to exit (no deadline: the program's lifetime is the user's, D103)";

/// The launcher's file name, beside this binary (crates/blaze-launcher, `[[bin]] name`).
pub(crate) const LAUNCHER: &str = "blaze-launcher";

/// The program's working directory: the command's own (`.` as the start resolves it in this
/// process), as `cargo run` and every shell give a program.
const WORKING_DIR: &str = ".";

/// Why `blaze run` could not run the program in a process of its own. Every one comes before
/// the program's first instruction, or is the parent's own failure while serving it.
#[derive(Debug)]
pub enum OwnError {
    /// Laying out its records, or the records folder.
    Parent(ParentError),
    /// Starting it, serving it, or the parent's signals (D103).
    Loop(LoopError),
    /// The command's world lacks what a run needs (`env`, `fs`, `host`, `exe`, `spawner`).
    Missing(Missing),
    /// This binary's own path has no folder to find the launcher in.
    NoLauncherFolder { exe: std::boxed::Box<str, std::alloc::Global> },
}

impl fmt::Display for OwnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OwnError::Parent(error) => write!(f, "{error}"),
            OwnError::Loop(error) => write!(f, "{error}"),
            OwnError::Missing(missing) => write!(f, "`blaze run` did not start the program (run.md piece 6): {missing}"),
            OwnError::NoLauncherFolder { exe } => {
                write!(f, "`blaze run` did not start the program (run.md piece 6): this binary's path `{exe}` has no folder, and `{LAUNCHER}` is looked for beside it")
            }
        }
    }
}

impl core::error::Error for OwnError {}

/// `{form}:{64 hex digits}`, the launcher's `BLAZE_LAUNCH_ENTRY` (blaze-launch-rt run_mode.rs
/// `Start::parse`): `c-main` for the C entry shim `main(argc, argv)`, the Rust start.
pub(crate) fn c_main_line(symbol: &[u8; 32]) -> [u8; 71] {
    let mut out = [0u8; 71];
    out[..7].copy_from_slice(b"c-main:");
    out[7..].copy_from_slice(&run_parent::hex(symbol));
    out
}

/// The launcher's path: [`LAUNCHER`] in the folder of `exe`, this binary's own path.
fn launcher_beside<'f>(exe: &str, frame: &'f Frame) -> Result<bstd::mem::String<&'f Frame>, OwnError> {
    let Some((folder, _)) = exe.rsplit_once('/') else {
        return Err(OwnError::NoLauncherFolder { exe: std::boxed::Box::from(exe) });
    };
    Ok(bstd::mem::format_in!(frame, "{folder}/{LAUNCHER}"))
}

/// Runs `files` (the root package `package`'s sources, as the front end takes them with `new`)
/// in a process of its own under the run parent: the program's status. See the module's doc.
pub(crate) fn run_own(cx: &Cx, launch: &Launch<'_>, files: &[SourceFile<'_>], new: NewFront<'_>, package: &str, frame: &Frame) -> Result<u8, OwnError> {
    let io = launch.io;
    let env = io.need_env().map_err(OwnError::Missing)?;
    let fs = io.need_fs().map_err(OwnError::Missing)?;
    let host = io.need_host().map_err(OwnError::Missing)?;
    // 1. The records, under the home the command was started with (rule 75: read here, at the
    // edge, from the command's own environment).
    let home_name = VarName::new("HOME").map_err(|_| OwnError::Parent(ParentError::NoHome))?;
    let home = env.get(&home_name).ok().flatten().ok_or(OwnError::Parent(ParentError::NoHome))?;
    let records = Path::new(RECORDS).map_err(|why| OwnError::Parent(ParentError::Path(why)))?;
    let folder = fs.home(cx, home, &records).map_err(|why| OwnError::Parent(ParentError::Home(why)))?;
    let erased = blaze_own::Erased::new(frame);
    let alloc = erased.handle();
    let mut laid = run_parent::lay_out(cx, files, new, &folder, alloc).map_err(OwnError::Parent)?;
    let store = bstd::mem::format_in!(frame, "{}/{RECORDS}", home.trim_end_matches('/'));
    let entry = c_main_line(&laid.entry);
    // The digits and the form are ASCII: always UTF-8.
    let entry = core::str::from_utf8(&entry).map_err(|_| OwnError::Parent(ParentError::NoEntry))?;
    // 2. The launcher.
    let exe = host.exe().map_err(OwnError::Missing)?;
    let launcher = launcher_beside(&exe, frame)?;
    // 3. Its environment: the command's own, then the run mode's three.
    let mut vars: Vec<(&str, &str), &Frame> = Vec::new_in(frame);
    run_parent::launch_env(env.each(), store.as_str(), entry, &mut vars);
    let spawner = host.spawner().map_err(OwnError::Missing)?;
    let started = run_loop::Launch { launcher: launcher.as_str(), arg0: package, args: launch.args, env: &vars, dir: WORKING_DIR };
    // 4. The loop, in the span that names its one exempt wait.
    let cx = cx.span(PROGRAM_SPAN);
    let ran = run_loop::run_program(&cx, &spawner, &started, &mut laid.needs, alloc).map_err(OwnError::Loop)?;
    Ok(ran.status)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prints "own process: c-main entry line and the launcher beside the binary". The
    /// launcher reads `c-main:<64 hex>` (run_mode.rs `Start::parse`: "the form `unit` or
    /// `c-main`"); a line in the `unit` form would call the shim with no `argc` and `argv`.
    #[test]
    fn the_entry_line_is_c_main_and_the_launcher_is_beside() {
        let line = c_main_line(&[0xab; 32]);
        let text = core::str::from_utf8(&line).expect("ASCII");
        assert!(text.starts_with("c-main:"), "{text}");
        assert_eq!(text.len(), 7 + 64);
        assert!(text[7..].bytes().all(|byte| byte == b'a' || byte == b'b'), "{text}");
        let frame = Frame::new(bstd::mem::Tag::Other).expect("a frame");
        assert_eq!(launcher_beside("/opt/blaze/bin/blaze", &frame).expect("a folder").as_str(), "/opt/blaze/bin/blaze-launcher");
        assert!(matches!(launcher_beside("blaze", &frame), Err(OwnError::NoLauncherFolder { .. })));
        println!("own process: c-main entry line and the launcher beside the binary");
    }
}
