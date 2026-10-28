//! blaze's cargo: the commands, the daemon and its client, the plans of builds, the sandbox and
//! the remote store. The engine (crates/blaze-engine) compiles and links; its modules of the same
//! names (`check`, `doctest`, `link`, ...) take what is here of them and add the rest.

#![allow(clippy::print_stderr, reason = "a command line program: stderr is where it speaks")]

use std::path::{Path, PathBuf};

use blaze_in_place as splice;

pub mod answers;
pub mod as_rustc;
pub mod cargo;
pub mod check;
pub mod content;
pub mod dep_info;
pub mod deps;
pub mod doctest;
pub mod engine;
pub mod errors;
pub mod events;
pub mod link;
pub mod live;
pub mod memory;
pub mod plan;
pub mod process;
pub mod remote;
pub mod renamed;
pub mod sandbox;
pub mod socket;
pub mod stub;
pub mod symbols;
pub mod trace;
pub mod tree;
pub mod unstable;
pub mod weaken;

/// How many of the functions blaze could not compile an error names.
const REFUSED_SHOWN: usize = 16;
/// What the first line of [`refused_error`] says, which the daemon's client finds it by.
pub const REFUSED_SAYS: &str = "blaze could not compile";

/// Why a program with nothing of rustc's is not built: the functions it reaches that blaze
/// could not compile, `reached` of them, each `name: why`, the first [`REFUSED_SHOWN`] named.
/// Nothing else compiles them (AGENTS.md, no fallback), and a link without them would fail on
/// what they leave undefined or keep a stand-in that aborts.
pub fn refused_error(program: &str, reached: usize, said: impl Iterator<Item = String>) -> String {
    let mut text = format!(
        "`{program}` reaches {reached} function{} {REFUSED_SAYS}, and a build with nothing of \
         rustc's has nothing else to compile {}, so it is not built:",
        if reached == 1 { "" } else { "s" },
        if reached == 1 { "it" } else { "them" },
    );
    for line in said.take(REFUSED_SHOWN) {
        text += &format!("\n  {line}");
    }
    if reached > REFUSED_SHOWN {
        text += &format!("\n  and {} more", reached - REFUSED_SHOWN);
    }
    text
}

/// The lock, also after a thread panicked with it: a build that is interrupted or that
/// panics unwinds, and what a lock here guards is whole between any two statements, so the
/// requests after it go on with what is there.
pub fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Has a command that prints for a reader end quietly when the reader stops reading,
/// instead of failing on the closed pipe. Not for a process that writes to sockets.
pub fn end_with_reader() {
    #[cfg(unix)]
    // SAFETY: restores the default action of a signal, before any thread is started.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

/// `blaze 0.20261028.9 (fb82b09 2026-10-28)` for a release, `blaze 0.0.0-dev (fb82b09)` for a
/// build of the tree: the engine's (`engine::Engine::version_line`, from its build.rs).
pub fn version() -> String {
    engine::get().version_line.to_owned()
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

/// The target of a build script whose commands run on the host (`--run-tool`).
pub const RUN_TOOL_TARGET: &str = "wasm32-wasip1";

/// Has `bytes` be the file at `path`. A file that was there is left as it is for whoever has
/// it open or under another name: the object of a program can be that of another one too.
pub fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut new = path.as_os_str().to_owned();
    new.push(format!(".{}.new", std::process::id()));
    std::fs::write(&new, bytes)?;
    std::fs::rename(&new, path)
}
