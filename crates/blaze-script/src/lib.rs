//! What a repository script needs, and nothing of blaze's (docs/v2/plan/design/scripts.md,
//! "Tier 0"): the pinned released blaze builds this crate and the scripts of `scripts/`, so it
//! depends on no crate of `crates/` and uses no feature that release lacks.
//!
//! - [`input`]: a script's one typed input (AGENTS.md rule scripts-take-typed-input), with the
//!   refusals `tools/typed-input` gave, word for word.
//! - [`pyjson`]: JSON written as Python's `json.dumps` writes it, so a generated file comes out
//!   byte for byte as the Python script wrote it.
//! - [`run`], [`git`]: a program run with its output captured.
//! - [`fail`], [`expect`]: how a script ends in an error, and how its `selftest` checks.
//!
//! When a release carries bstd and reflection, a script's `Input::parse` is derived
//! (`bstd::data`) and these become bstd's (`blaze::std_use` moves the callers).

pub mod input;
pub mod pyjson;

use std::{
    path::Path,
    process::{Command, Stdio},
};

pub use regex;
pub use serde_json::{self as json, Map, Value};
pub use toml;

/// Ends the script as Python's `sys.exit(f"{script}: {message}")` does: the line on standard
/// error, exit status 1.
pub fn fail(script: &str, message: &str) -> ! {
    eprintln!("{script}: {message}");
    std::process::exit(1)
}

/// A `selftest`'s one check: when `holds` is false the script ends with
/// `<script> selftest: <what>`, exit status 1.
pub fn expect(script: &str, holds: bool, what: &str) {
    if !holds {
        fail(&format!("{script} selftest"), what);
    }
}

/// What a program that ran left.
#[derive(Debug, Clone)]
pub struct Ran {
    /// The exit status; -1 when a signal ended it.
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Ran {
    /// Standard output as text, bytes that are not UTF-8 replaced.
    pub fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Standard error as text, bytes that are not UTF-8 replaced.
    pub fn err(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Runs `argv` in `cwd` with no standard input and both outputs captured. `Err` is the program
/// that could not be started, with why.
pub fn run(argv: &[&str], cwd: &Path) -> Result<Ran, String> {
    let (program, args) = argv.split_first().ok_or_else(|| "no program to run".to_owned())?;
    // The workspace's `toolchain::command` is a crate of blaze's; this crate may depend on none.
    #[allow(clippy::disallowed_methods, reason = "the one place a script starts a program; `cwd` is always given")]
    let mut command = Command::new(program);
    let done = command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{program} did not start in {}: {e}", cwd.display()))?;
    Ok(Ran { status: done.status.code().unwrap_or(-1), stdout: done.stdout, stderr: done.stderr })
}

/// `git <args>` in `root`: its standard output, or `Err` with what it said when it failed.
pub fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let mut argv = vec!["git"];
    argv.extend_from_slice(args);
    let ran = run(&argv, root)?;
    match ran.status {
        0 => Ok(ran.out()),
        status => Err(format!("git {} exited {status}: {}", args.join(" "), ran.err().trim())),
    }
}

/// A file's text as Python's `open(path, encoding="utf-8").read()` gives it (`\r\n` and `\r`
/// become `\n`), or `None` when there is no such file, it is a directory or it is not UTF-8.
pub fn read_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    Some(if text.contains('\r') { text.replace("\r\n", "\n").replace('\r', "\n") } else { text })
}
