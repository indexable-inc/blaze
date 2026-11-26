//! Sans-IO request/answer protocol between [`crate::machine::Machine`] and its host driver.
//!
//! `blaze-sh` never touches the filesystem, clock, environment, or OS processes directly: every
//! external effect comes out as a [`Request`] from [`Step::Need`] and resumes via
//! `Machine::answer(Answer)`.

use blaze_own::{ARc, AString, AVec};

use crate::error::ShellError;
use crate::state::EngineState;
use crate::value::{Bytes, DateTime, Value};

/// One step produced by driving [`crate::machine::Machine::step`].
#[derive(Debug)]
pub enum Step<'a> {
    /// Evaluation finished (either a value or an error) with the committed state.
    Done(EvalOutcome<'a>),
    /// Evaluation needs the host to fulfill an external IO / clock / process / Rust-cell request.
    Need(Request<'a>),
    /// Incremental output emitted before completion (e.g. `print`).
    Stream(StreamChunk<'a>),
}

/// Final outcome of executing a submitted line.
#[derive(Clone, Debug)]
pub struct EvalOutcome<'a> {
    pub value: Value<'a>,
    pub state: EngineState<'a>,
    pub exit_code: i32,
    /// When the line itself was an interactive external command rendered on a PTY terminal block.
    pub terminal_passthrough: bool,
}

/// Incremental chunk emitted during evaluation.
#[derive(Clone, Debug)]
pub enum StreamChunk<'a> {
    Value(Value<'a>),
    Text { text: AString<'a>, newline: bool },
}

/// How an external command should run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExternalMode {
    /// Captured stdout/stderr piped into the next shell stage.
    Capture,
    /// Final stage attached to a PTY / `blaze-vt` terminal block in `blaze-shell`.
    Interactive,
}

/// Filesystem mutation requested by `cp`, `mv`, `rm`, `mkdir`, `touch`.
#[derive(Clone, Debug)]
pub enum FsOp<'a> {
    Cp { src: AString<'a>, dst: AString<'a>, recursive: bool },
    Mv { src: AString<'a>, dst: AString<'a> },
    Rm { path: AString<'a>, recursive: bool, force: bool },
    Mkdir { path: AString<'a>, parents: bool },
    Touch { path: AString<'a> },
}

/// Effect requested by the sans-IO shell machine.
#[derive(Clone, Debug)]
pub enum Request<'a> {
    ReadDir {
        path: AString<'a>,
        show_hidden: bool,
    },
    Stat {
        path: AString<'a>,
    },
    ReadFile {
        path: AString<'a>,
    },
    WriteFile {
        path: AString<'a>,
        bytes: AVec<'a, u8>,
        append: bool,
    },
    FsOp(FsOp<'a>),
    ChangeDir {
        path: AString<'a>,
    },
    Which {
        name: AString<'a>,
    },
    Now,
    SysInfo,
    ProcessList,
    SpawnExternal {
        program: AString<'a>,
        args: AVec<'a, AString<'a>>,
        cwd: AString<'a>,
        env: AVec<'a, (ARc<'a, str>, ARc<'a, str>)>,
        stdin: Option<AVec<'a, u8>>,
        mode: ExternalMode,
    },
    /// Execute a live Rust cell (`rust { ... }` or `:rs ...`) via blaze.
    EvalRust {
        prior_cells: AVec<'a, ARc<'a, str>>,
        code: AString<'a>,
        input_json: AString<'a>,
        cwd: AString<'a>,
    },
    /// Request the host to fork the active session branch (returned as branch name/id).
    ForkSession {
        label: Option<AString<'a>>,
    },
}

/// Entry returned by [`Answer::DirEntries`].
#[derive(Clone, Debug)]
pub struct DirEntry<'a> {
    pub name: AString<'a>,
    pub kind: FileKind,
    /// Optional rich file/directory classification from `blaze-sniff` (e.g. `"Git worktree"`, `"Rust crate"`).
    pub analysis: Option<AString<'a>>,
    pub size: Bytes,
    pub modified: DateTime,
    pub readonly: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

impl FileKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            FileKind::File => "file",
            FileKind::Dir => "dir",
            FileKind::Symlink => "symlink",
            FileKind::Other => "other",
        }
    }
}

/// Process entry returned by [`Answer::Processes`].
#[derive(Clone, Debug)]
pub struct ProcEntry<'a> {
    pub pid: u32,
    pub ppid: u32,
    pub name: AString<'a>,
    pub status: AString<'a>,
    pub cpu_pct: f64,
    pub mem: Bytes,
}

/// System summary returned by [`Answer::Sys`].
#[derive(Clone, Debug)]
pub struct SysSnapshot<'a> {
    pub host: AString<'a>,
    pub os: AString<'a>,
    pub arch: AString<'a>,
    pub cpu_count: u32,
    pub mem_total: Bytes,
    pub mem_used: Bytes,
    pub uptime_ns: i64,
}

/// Result of running an external command.
#[derive(Clone, Debug)]
pub struct ExternalResult<'a> {
    pub exit_code: i32,
    pub stdout: AVec<'a, u8>,
    pub stderr: AVec<'a, u8>,
    /// True if the host attached stdout to an interactive `blaze-vt` terminal block.
    pub interactive: bool,
}

/// Result of compiling and running a `rust { ... }` cell via blaze.
#[derive(Clone, Debug)]
pub struct RustEvalResult<'a> {
    pub ok: bool,
    /// Structured JSON value returned by the cell (or raw stdout if not structured).
    pub output_json: Option<AString<'a>>,
    pub stdout: AString<'a>,
    pub stderr: AString<'a>,
    pub compile_ms: u64,
    pub run_ms: u64,
}

/// Host's reply to a [`Request`].
#[derive(Clone, Debug)]
pub enum Answer<'a> {
    DirEntries(AVec<'a, DirEntry<'a>>),
    Stat(DirEntry<'a>),
    FileBytes(AVec<'a, u8>),
    Ok,
    ChangedDir(AString<'a>),
    FoundPath(Option<AString<'a>>),
    Now(DateTime),
    Sys(SysSnapshot<'a>),
    Processes(AVec<'a, ProcEntry<'a>>),
    External(ExternalResult<'a>),
    Rust(RustEvalResult<'a>),
    Forked(AString<'a>),
    Err(ShellError<'a>),
}

/// Metadata for a builtin command (single source of truth for `help` and editor autocomplete).
#[derive(Clone, Copy, Debug)]
pub struct BuiltinSpec {
    pub name: &'static str,
    pub signature: &'static str,
    pub summary: &'static str,
    pub flags: &'static [&'static str],
}
