//! THE CALLS a compile-time process makes of blaze, and blaze's answers: every reach of the
//! system the wasm path serves today (comptime-native.md, "What exists", part 1), as one typed
//! enum each way. The guest's standard library (its `sys` layer, comptime-native.md "The std
//! backend") turns each libc-level operation into one of these; nothing else crosses.
//!
//! Lockstep: the guest makes one call and waits for its answer (a compile-time process has one
//! thread: [`Call::Thread`] is refused), so the protocol has one call in flight, which is why
//! the host's machine has one `Answering` state and no table of calls.

#![blaze::sans_io]

/// A list of words (arguments, `NAME=value` pairs, directory names), each ended by a NUL: no
/// word of these kinds holds a NUL, so the form needs no lengths. Checked by [`Words::new`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Words<'a>(pub(crate) &'a [u8]);

impl<'a> Words<'a> {
    /// No words.
    pub const EMPTY: Words<'static> = Words(&[]);

    /// `bytes` when empty or ended by a NUL.
    pub fn new(bytes: &'a [u8]) -> Option<Words<'a>> {
        if bytes.is_empty() || bytes.last() == Some(&0) { Some(Words(bytes)) } else { None }
    }

    pub fn as_bytes(self) -> &'a [u8] {
        self.0
    }

    /// The words, in order.
    pub fn iter(self) -> impl Iterator<Item = &'a [u8]> {
        let body = match self.0.split_last() {
            Some((_, body)) => body,
            None => &[][..],
        };
        body.split(|b| *b == 0).filter(move |_| !self.0.is_empty())
    }

    pub fn count(self) -> usize {
        self.0.iter().filter(|b| **b == 0).count()
    }
}

/// Which standard stream a write goes to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stream {
    Out,
    Err,
}

/// Which clock is read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClockKind {
    /// `SystemTime::now` (`CLOCK_REALTIME`; WASI clock 0).
    Wall,
    /// `Instant::now` (`CLOCK_MONOTONIC`; WASI clock 1), and the CPU-time clocks.
    Monotonic,
}

/// What a file lookup wants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Want {
    /// The bytes, whole (`open` then `read`: the guest's library holds the bytes and serves
    /// `read`, `seek` and `tell` from them: one crossing per file).
    Read,
    /// Kind and length (`stat`, `path_filestat_get`).
    Stat,
    /// The names in a directory (`opendir`/`readdir`, `fd_readdir`).
    List,
}

/// A guest's call. Paths are as the guest spelled them (absolute, or relative to its working
/// directory); the host maps them through the grants' mounts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Call<'f> {
    /// The first frame: the guest's protocol version ([`crate::PROTOCOL`]).
    Hello { protocol: u32 },
    /// A file or directory looked up (`path_open`, `fd_read`, `path_filestat_get`, `fd_readdir`).
    Look { path: &'f [u8], want: Want },
    /// One variable (`getenv`; the macro shim's `env_var`).
    Env { name: &'f [u8] },
    /// Every variable (`environ_get`).
    EnvAll,
    /// The arguments (`args_get`).
    Args,
    /// The working directory (the macro shim's `current_dir`; `getcwd`).
    Cwd,
    /// A clock read (`clock_time_get`).
    Clock { which: ClockKind },
    /// Random bytes (`random_get`, `getrandom`, std's `HashMap` keys).
    Random { len: u32 },
    /// A write to standard output or error (`fd_write` on 1 or 2).
    Write { stream: Stream, bytes: &'f [u8] },
    /// Bytes written into a file at `at`, made if absent (`path_open` with create, `fd_write`,
    /// `fd_pwrite`): the output root only.
    WriteFile { path: &'f [u8], at: u64, bytes: &'f [u8] },
    /// A file's length set (`fd_filestat_set_size`, `O_TRUNC`): the output root only.
    SetLen { path: &'f [u8], len: u64 },
    /// A directory made (`path_create_directory`): the output root only.
    CreateDir { path: &'f [u8] },
    /// A file or empty directory removed (`path_unlink_file`, `path_remove_directory`).
    Remove { path: &'f [u8] },
    /// A rename within the output root (`path_rename`).
    Rename { from: &'f [u8], to: &'f [u8] },
    /// A sleep or a wait on a timer (`poll_oneoff`, `nanosleep`).
    Sleep { ns: u64 },
    /// A thread started (`pthread_create`): refused, there is one thread.
    Thread,
    /// A socket opened (`socket`, `getaddrinfo`): refused, there is no network.
    Socket,
    /// A command run (std's `Command`: the script process shim's `spawn`), answered by the
    /// build script host's routing (the probe table, `git`, derived facts, the C mode).
    Spawn { program: &'f [u8], args: Words<'f>, env: Words<'f>, cwd: &'f [u8] },
    /// A proc-macro bridge call to blaze's token server: `method` is the tag of
    /// `blaze_proc_macro::protocol::Method` (that enum is the one home of the 31 methods), `args`
    /// its arguments in the bridge's own wire (blaze-proc-macro wire.rs). The two methods that
    /// reach the world are not sent this way: they are [`Call::TrackEnv`] and
    /// [`Call::TrackPath`], so the host records them.
    Bridge { method: u8, args: &'f [u8] },
    /// `proc_macro::tracked_env::var`: a variable the expansion depends on, with the value the
    /// macro saw.
    TrackEnv { name: &'f [u8], value: Option<&'f [u8]> },
    /// `proc_macro::tracked_path::path`: a file the expansion depends on.
    TrackPath { path: &'f [u8] },
    /// A proc macro's output stream, in the bridge's wire (the shim's `output`): once, and the
    /// run's end.
    Return { output: &'f [u8] },
    /// The process exits (`proc_exit`, `exit`): a build script's end; a proc macro may not.
    Exit { code: i32 },
    /// A C function the system layer's table refuses by name (blaze-comptime-sys table.rs: a
    /// `chmod`, `setuid`, `kill`, `dlopen`), sent so the host records the finding; the guest's
    /// library answers the C caller with the table's `errno`, whatever the host says.
    Denied { name: &'f [u8] },
}

/// A call's kind, for states, counts and refusals.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallKind {
    Hello,
    Look,
    Env,
    EnvAll,
    Args,
    Cwd,
    Clock,
    Random,
    Write,
    WriteFile,
    SetLen,
    CreateDir,
    Remove,
    Rename,
    Sleep,
    Thread,
    Socket,
    Spawn,
    Bridge,
    TrackEnv,
    TrackPath,
    Return,
    Exit,
    Denied,
}

impl Call<'_> {
    pub fn kind(&self) -> CallKind {
        match self {
            Call::Hello { .. } => CallKind::Hello,
            Call::Look { .. } => CallKind::Look,
            Call::Env { .. } => CallKind::Env,
            Call::EnvAll => CallKind::EnvAll,
            Call::Args => CallKind::Args,
            Call::Cwd => CallKind::Cwd,
            Call::Clock { .. } => CallKind::Clock,
            Call::Random { .. } => CallKind::Random,
            Call::Write { .. } => CallKind::Write,
            Call::WriteFile { .. } => CallKind::WriteFile,
            Call::SetLen { .. } => CallKind::SetLen,
            Call::CreateDir { .. } => CallKind::CreateDir,
            Call::Remove { .. } => CallKind::Remove,
            Call::Rename { .. } => CallKind::Rename,
            Call::Sleep { .. } => CallKind::Sleep,
            Call::Thread => CallKind::Thread,
            Call::Socket => CallKind::Socket,
            Call::Spawn { .. } => CallKind::Spawn,
            Call::Bridge { .. } => CallKind::Bridge,
            Call::TrackEnv { .. } => CallKind::TrackEnv,
            Call::TrackPath { .. } => CallKind::TrackPath,
            Call::Return { .. } => CallKind::Return,
            Call::Exit { .. } => CallKind::Exit,
            Call::Denied { .. } => CallKind::Denied,
        }
    }
}

/// The error a guest's library turns into the one the real system gives (an `io::ErrorKind`,
/// an `errno`): what an existing crate that probes sees, as it sees today.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SysError {
    /// `ENOENT`: no such file (also every path outside the grants: nothing outside them exists).
    NotFound,
    /// `ENOTDIR`.
    NotADir,
    /// `EISDIR`.
    IsADir,
    /// `EROFS`: a write anywhere but the output root.
    ReadOnly,
    /// `ENOTSUP`: threads, sockets.
    Unsupported,
    /// `EINVAL`: a malformed request (a path the guest's library should not have sent).
    Invalid,
    /// `ENOSPC`: the output root is at its cap.
    NoSpace,
    /// `EFBIG`: a file past the frame's cap.
    TooLarge,
    /// `EEXIST`.
    Exists,
    /// `ENOTEMPTY`: a directory removed with something in it.
    NotEmpty,
}

/// What a lookup found, as the guest is told.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileKind {
    File,
    Dir,
}

/// The host's answer to one call. `Return` and `Exit` are not answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer<'a> {
    Hello { protocol: u32 },
    /// [`Want::Read`]: the bytes.
    File { bytes: &'a [u8] },
    /// [`Want::Stat`].
    Stat { kind: FileKind, len: u64 },
    /// [`Want::List`]: the names, sorted bytewise.
    Listing { names: Words<'a> },
    /// [`Call::Env`] and [`Call::Cwd`]: the value, or `None` for an unset variable.
    Value { value: Option<&'a [u8]> },
    /// [`Call::EnvAll`] (`NAME=value` words) and [`Call::Args`].
    Words { words: Words<'a> },
    /// [`Call::Clock`].
    Clock { ns: u64 },
    /// [`Call::Random`].
    Random { bytes: &'a [u8] },
    /// A write, a file operation, a sleep, a track: done.
    Done,
    /// [`Call::Spawn`]: the command's status and streams.
    Spawned { status: i32, stdout: &'a [u8], stderr: &'a [u8] },
    /// [`Call::Bridge`]: the token server's reply, in the bridge's wire.
    Bridge { reply: &'a [u8] },
    /// The call failed as the real system's would.
    Error(SysError),
}
