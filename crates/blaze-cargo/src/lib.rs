//! blaze's cargo: the commands, the daemon and its client, the plans of builds, the sandbox and
//! the remote store. The engine (crates/blaze-engine) compiles and links; its modules of the same
//! names (`check`, `doctest`, `link`, ...) take what is here of them and add the rest.

#![allow(clippy::print_stderr, reason = "a command line program: stderr is where it speaks")]
#![deny(unsafe_code)]
#![feature(random)]

use std::path::{Path, PathBuf};

pub mod answers;
pub mod as_rustc;
/// blaze v2's CLI: a client of the agent API with cargo's flags, output and exit codes
/// (docs/v2/plan/lanes/cargo/notes.md section 7).
pub mod cli;
pub mod content;
pub mod directives;
#[allow(unsafe_code, reason = "the kernel calls std has no wrapper for, behind safe functions")]
pub mod raw;
pub mod release;
#[allow(unsafe_code, reason = "the sandbox's own system calls")]
pub mod sandbox;
/// blaze v2's daemon: the plane, the agent API's server and session, the VFS, the database and
/// the store in one loop (docs/v2/plan/lanes/cargo/notes.md section 6).
#[allow(unsafe_code, reason = "the daemon's own signal and process calls")]
pub mod serve;
pub mod socket;
pub mod toolchain;
pub mod unstable;

/// How many of the functions blaze could not compile an error names when the request does not
/// say ([`blaze_api::Cargo::refused`], `--refused`).
pub const REFUSED_SHOWN: u32 = 16;
/// What the first line of [`refused_error`] says, which the daemon's client finds it by.
pub const REFUSED_SAYS: &str = "blaze could not compile";

/// Why a program with nothing of rustc's is not built: the functions it reaches that blaze
/// could not compile, `reached` of them, each `name: why`, the first `shown` named (every one
/// with `None`). Nothing else compiles them (AGENTS.md, no fallback), and a link without them
/// would fail on what they leave undefined or keep a stand-in that aborts.
pub fn refused_error(program: &str, reached: usize, said: impl Iterator<Item = String>, shown: Option<u32>) -> String {
    let shown = shown.map_or(usize::MAX, |it| usize::try_from(it).unwrap_or(usize::MAX));
    let mut text = format!(
        "`{program}` reaches {reached} function{} {REFUSED_SAYS}, and a build with nothing of \
         rustc's has nothing else to compile {}, so it is not built:",
        if reached == 1 { "" } else { "s" },
        if reached == 1 { "it" } else { "them" },
    );
    for line in said.take(shown) {
        text += &format!("\n  {line}");
    }
    if reached > shown {
        text += &format!("\n  and {} more (`--refused all` names every one)", reached - shown);
    }
    text
}

#[cfg(test)]
mod refused_tests {
    #[test]
    fn a_refused_error_names_as_many_as_asked() {
        let said = || (0..20).map(|it| format!("f{it}: why"));
        let first = super::refused_error("app", 20, said(), Some(super::REFUSED_SHOWN));
        assert!(first.contains("\n  f15: why\n  and 4 more (`--refused all` names every one)"), "{first}");
        assert!(!first.contains("f16"), "{first}");
        let every = super::refused_error("app", 20, said(), None);
        assert!(every.ends_with("\n  f19: why") && !every.contains("more"), "{every}");
        let none = super::refused_error("app", 20, said(), Some(0));
        assert!(none.ends_with("so it is not built:\n  and 20 more (`--refused all` names every one)"), "{none}");
    }
}

/// The lock, also after a thread panicked with it: a build that is interrupted or that
/// panics unwinds, and what a lock here guards is whole between any two statements, so the
/// requests after it go on with what is there.
pub fn lock<T>(mutex: &parking_lot::Mutex<T>) -> parking_lot::MutexGuard<'_, T> {
    mutex.lock()
}

/// Has a command that prints for a reader end quietly when the reader stops reading,
/// instead of failing on the closed pipe. Not for a process that writes to sockets.
pub fn end_with_reader() {
    #[cfg(unix)]
    raw::process::die_on_closed_pipe();
}

/// `blaze 0.20261028.9 (fb82b09 2026-10-28)` for a release, `blaze 0.0.0-dev (fb82b09)` for a
/// build of the tree ([`release::Release::line`]).
pub fn version() -> String {
    release::get().line.to_owned()
}

/// What a worker that was started before its build was known is told on its standard input.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Told {
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub cwd: PathBuf,
    /// The build is one cargo has only started: these are the workspace and its crates, and
    /// what cargo wrote is in the arguments of the line that follows.
    #[serde(default)]
    pub ahead: bool,
}

/// Has `bytes` be the file at `path`. A file that was there is left as it is for whoever has
/// it open or under another name: the object of a program can be that of another one too.
pub fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut new = path.as_os_str().to_owned();
    new.push(format!(".{}.new", std::process::id()));
    std::fs::write(&new, bytes)?;
    std::fs::rename(&new, path)
}
