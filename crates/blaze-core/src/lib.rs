//! blaze's sans-IO core (docs/v2/plan/lanes/core.md section 3, docs/v2/plan/lanes/core/plane.md).
//!
//! A state machine of blaze's (the build's plane, a journal, the store's index) does no IO,
//! reads no clock and starts no thread. It asks for IO by pushing [`Effect`]s, each with a
//! [`Tag`], and is told how each went by a [`Completion`] with the same tag. A [`Driver`] runs
//! the effects: [`sim::Sim`] in one thread under a seeded scheduler that reorders, fails and
//! crashes (deterministic simulation tests), [`threads::Threads`] on the real file system, and
//! on Linux [`uring::Uring`], on one io_uring.
//!
//! Files are named relative to the driver's root, so the same state machine runs in a
//! simulation, a temporary directory or the daemon's state directory unchanged.
//!
//! On macOS [`kqueue::Kqueue`] keeps sockets, processes and FSEvents' watches on one kqueue,
//! the file calls on threads, and timers in a [`wheel::Wheel`] its wait ends at, as every
//! driver does. A process of [`Effect::Spawn`] in a sandbox is made
//! by the [`Confine`] a driver is given; a driver given none refuses it.
//!
//! On Windows [`iocp::Iocp`]. Each target's is its [`Native`] driver.
//!
//! Unsafe code is the kernel's and the mappings' only (docs/v2/plan/design/unsafe.md): each
//! driver of a kernel interface is one module allowed it below, as are `fs::mapped`,
//! `process::spawn` and `raw`'s single calls; the rest of the crate, the threads driver and
//! the simulation with it, is safe Rust.

#![deny(unsafe_code)]

pub mod busy;
#[cfg(target_os = "linux")]
mod cgroup;
pub mod create_process;
pub mod decimal;
#[cfg(target_os = "linux")]
#[allow(unsafe_code, reason = "the fanotify driver: its system calls and the events it reads")]
mod fanotify;
#[cfg(target_os = "macos")]
mod footprint;
pub mod fs;
#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "the FSEvents driver: CoreFoundation, FSEvents and dispatch calls")]
mod fsevents;
mod handoff;
#[cfg(windows)]
#[allow(unsafe_code, reason = "the IOCP driver: Win32 calls and the OVERLAPPEDs the kernel writes")]
pub mod iocp;
mod journal;
#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "the kqueue driver: kevent and the records it reads")]
pub mod kqueue;
#[cfg(unix)]
#[allow(unsafe_code, reason = "sockets: descriptors owned from raw and the calls std lacks")]
mod net;
#[cfg(unix)]
mod process;
#[allow(unsafe_code, reason = "single system calls and the aligned-copy view, each behind a safe function")]
mod raw;
pub mod signal;
pub mod sim;
mod sync;
mod tags;
pub mod threads;
#[cfg(target_os = "linux")]
#[allow(unsafe_code, reason = "the io_uring driver: its mapped rings, buffers and system calls")]
pub mod uring;
#[cfg(target_os = "linux")]
#[allow(unsafe_code, reason = "the inotify driver: its system calls and the events it reads")]
mod watch;
pub mod utf8;
pub mod wheel;

use std::{fmt, time::Duration};

pub use crate::journal::{Cursor, Journaled, Lost, Names, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG, Stamp};
pub use crate::tags::Tags;

/// Makes the sandbox of a process: given to a driver by whoever owns the platform's sandbox
/// (`blaze-usercode`), so the driver starts what it says in place of the program. A driver
/// given none fails every spawn whose [`Sandbox`] is on.
pub trait Confine: Send + Sync {
    /// What to start so that `command` runs in its sandbox, usually a launcher with the
    /// program after it. `scratch` is a directory of this run's own, empty, made by the
    /// driver and removed once [`Confine::traced`] has been asked: where the launcher keeps
    /// its record. An error says why no sandbox can be made here; nothing runs then.
    fn confine(&self, command: &Command, scratch: &std::path::Path) -> Result<Confined, String>;
    /// What the sandbox recorded the process read, once it has exited: asked only of a
    /// sandbox that traces.
    fn traced(&self, command: &Command, scratch: &std::path::Path) -> Result<Traced, String>;
    /// The warm launcher this sandbox hands its runs to, where it has one: the process a driver
    /// starts once, and again only after it ended, outside any sandbox and in no job of the
    /// driver's. Its standard input is a pipe the driver writes [`Confine::hand`]'s frames to,
    /// its standard output a pipe the driver reads [`Answer`]s from, and its standard error the
    /// driver's own. A driver given a confine that has one hands it every sandboxed spawn and
    /// never asks [`Confine::confine`] for one; a driver that can't hand a run fails the spawn,
    /// naming the launcher. Asked once, as the driver is given the confine.
    fn warm(&self) -> Option<Confined> {
        None
    }
    /// The frame of `command`'s run for the warm launcher, with `handed` naming what the driver
    /// passed it for the run: written whole, frames in the order of their spawns. `scratch` is
    /// as for [`Confine::confine`], and [`Confine::traced`] is asked as for a run it started.
    /// An error says why the run can't be handed; nothing runs then.
    fn hand(
        &self,
        command: &Command,
        scratch: &std::path::Path,
        handed: &Handed,
    ) -> Result<Vec<u8>, String> {
        _ = (command, scratch, handed);
        // alloc: an error path.
        Err("this sandbox has no warm launcher to hand a run to".to_owned())
    }
}

/// What a driver passed a warm launcher for one run ([`Confine::hand`]). On Windows each is a
/// handle the driver duplicated into the launcher's process, the launcher's from then on: the
/// standard ones inheritable, the job not. On Unix (the warm launcher of cargo's request 14)
/// each is the place, from 1, of its descriptor in the `SCM_RIGHTS` message the frame is sent
/// with. 0 is none.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Handed {
    /// The driver's name for the run, unique among its launcher's runs in flight, which the
    /// run's [`Answer`] carries back.
    pub run: u64,
    pub stdin: u64,
    pub stdout: u64,
    pub stderr: u64,
    /// The job its processes go in (Windows): made by the driver as for a spawn it starts
    /// itself (killed on close, a crash ending a process, the command's memory limit), and
    /// followed, killed and measured by the driver as that spawn's job is.
    pub job: u64,
}

/// What a warm launcher writes on its standard output as each run ends: [`Answer::LEN`]
/// bytes, the run and the code it ended with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Answer {
    pub run: u64,
    pub code: i32,
}

impl Answer {
    pub const LEN: usize = 12;

    /// The run, then the code, little-endian.
    pub fn to_bytes(self) -> [u8; Answer::LEN] {
        let mut out = [0; Answer::LEN];
        out[..8].copy_from_slice(&self.run.to_le_bytes());
        out[8..].copy_from_slice(&self.code.to_le_bytes());
        out
    }

    pub fn from_bytes(bytes: [u8; Answer::LEN]) -> Answer {
        let (run, code) = bytes.split_at(8);
        Answer {
            run: u64::from_le_bytes(run.try_into().expect("8 bytes")),
            code: i32::from_le_bytes(code.try_into().expect("4 bytes")),
        }
    }
}

/// The process a [`Confine`] starts for a command: its whole environment is this one.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: in blaze-cargo's Linux sandbox setup, once per spawn
pub struct Confined {
    pub program: std::path::PathBuf,
    pub args: Vec<std::ffi::OsString>,
    pub env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    pub dir: std::path::PathBuf,
}

/// Names a request so its completion can be matched to it. The state machine chooses tags; a
/// driver hands them back unchanged.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Tag(pub u64);

/// A file under the driver's root: `/`-separated, relative, without `.` or `..`. Shared, so
/// the name an asker hands each [`Effect`] of a file is a count, never a copy: two words.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)] // clone: a refcount, no allocation
pub struct FileName(std::sync::Arc<str>);

const _: () = assert!(size_of::<FileName>() == 2 * size_of::<usize>());

impl FileName {
    /// `None` for a name that would leave the driver's root or names nothing.
    pub fn new(name: &str) -> Option<FileName> {
        // alloc: once per name made; its clones share it
        FileName::is_valid(name).then(|| FileName(name.into()))
    }

    /// Whether [`FileName::new`] takes `name`, without making one.
    pub(crate) fn is_valid(name: &str) -> bool {
        !name.is_empty()
            && !name.starts_with('/')
            && name.split('/').all(|it| !it.is_empty() && it != "." && it != "..")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether `self` is `dir` or under it.
    pub fn is_under(&self, dir: &FileName) -> bool {
        self.0 == dir.0
            || self.0.strip_prefix(&*dir.0).is_some_and(|rest| rest.starts_with('/'))
    }
}

impl fmt::Display for FileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The most bytes one [`Effect::Recv`] appends, whatever its `most`: a receive reserves its
/// room before the bytes come, so an asker of a long frame grows its buffer by this much a
/// receive, not by the frame's length at once.
pub const RECV_MOST: usize = 64 << 10;

/// IO a state machine asks for.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: tests' logs of what was asked (blaze-vfs's); a driver takes each by value
pub enum Effect {
    /// The whole file. A file that is not there reads as empty: a journal not yet written is
    /// one with no records.
    ReadAll { file: FileName, tag: Tag },
    /// `len` bytes at `at`, or fewer at the end of the file.
    Read { file: FileName, at: u64, len: usize, tag: Tag },
    /// The target of the symbolic link `file`, never followed: completes with [`Done::Read`]
    /// of the target's bytes as the link holds them (on Windows UTF-8, `\` written `/`). A
    /// `file` that is not a symbolic link, or is not there, fails naming it.
    ReadLink { file: FileName, tag: Tag },
    /// Bytes added at the end of the file, creating it. Not durable until a [`Effect::Sync`] of
    /// the file asked for after this one has completed.
    Append { file: FileName, bytes: Vec<u8>, tag: Tag },
    /// Everything appended to the file before this was asked for is durable once it completes.
    Sync { file: FileName, tag: Tag },
    /// The file cut to its first `len` bytes (made if missing); durable once a later sync
    /// completes.
    Truncate { file: FileName, len: u64, tag: Tag },
    /// The file's whole contents replaced by `bytes`, durably and all at once: a reader or a
    /// crash sees the old contents or the new, never a part. Its directories are made.
    Write { file: FileName, bytes: Vec<u8>, tag: Tag },
    /// Every file and directory under `dir`, at any depth, sorted by name, each with its
    /// [`Meta`]. A symbolic link is listed as a file of its own length and not followed.
    /// Completes with [`Done::Listed`]; a `dir` that is not there fails.
    List { dir: FileName, tag: Tag },
    /// `bytes` written at `at` of `file`, which exists, past its end making it longer; durable
    /// once a later [`Effect::Sync`] of the file completes. Completes with [`Done::Written`].
    /// For a link writing the pages that changed into a clone of the last executable.
    WriteAt { file: FileName, at: u64, bytes: Vec<u8>, tag: Tag },
    /// `from` renamed over `to`, atomically: a reader sees the old `to` or the new, and the
    /// new name survives a crash once it completes. Completes with [`Done::Renamed`].
    Rename { from: FileName, to: FileName, tag: Tag },
    /// The file's permission bits set to `mode` (0o755 for an executable). Completes with
    /// [`Done::Moded`].
    Mode { file: FileName, mode: u32, tag: Tag },
    /// `dir` made the user's alone, and checked to be with everything under it, for what keeps
    /// code it runs (the store: a value is checked against its own hash, never against what its
    /// key says made it, so whoever may write there can plant code under any key). A missing
    /// `dir` is made with its parents, on Unix with mode 0o700. Then `dir` and every file and
    /// directory under it, links followed, is checked to be the user's with no one else able
    /// to write it ([`Access`]); once `dir` is, on Unix the group's and others' bits are taken
    /// off it, so that no one else reads what was built either. What is refused is never
    /// mended. Completes with [`Done::Private`].
    Private { dir: FileName, tag: Tag },
    /// The file's access and modification times both set to `modified_ns`, nanoseconds since
    /// the Unix epoch, as cargo gives a build script's `output` the time its run began. Of the
    /// file as it is when this runs, so asked once the write that made it has completed; a
    /// symbolic link's target. Durable once a later [`Effect::Sync`] of the file completes.
    /// Completes with [`Done::Timed`]. A file that is not there fails, and so does a time the
    /// system's call cannot say (past a 32-bit `time_t`), naming the file; what is kept is the
    /// file system's resolution of it (100 ns on NTFS, 1 s on HFS+).
    Times { file: FileName, modified_ns: u64, tag: Tag },
    /// `to` made a file with `from`'s contents that shares its blocks (`FICLONE`,
    /// `clonefile`). Fails where the file system cannot share blocks, naming it: nothing
    /// copies instead.
    Clone { from: FileName, to: FileName, tag: Tag },
    /// `to` made another name of `from`, for outputs nothing writes in place.
    Link { from: FileName, to: FileName, tag: Tag },
    /// What the file is, or that it is not there.
    Stat { file: FileName, tag: Tag },
    /// The file, or an empty directory, gone. Gone already is done.
    Remove { file: FileName, tag: Tag },
    /// Completes with [`Done::Changed`] each time files under `dir` change, until an
    /// [`Effect::Unwatch`] of the same tag, which ends it with [`Done::Unwatched`], or a
    /// [`Done::Failed`], which ends it too. It is in flight until it ends; an unwatch has no
    /// completion of its own but the watch's end. A change that names `dir` itself says the
    /// directory may have been replaced or moved (inotify, which watches an inode, says so of
    /// an overflow too): a watcher that must hear of every later change watches again.
    Watch { dir: FileName, tag: Tag },
    /// A watch of `dir` that starts where `since` left the system's journal of changes
    /// (docs/v2/plan/design/restore.md): it completes with [`Done::Journaled`] for the changes
    /// made since then, one with `caught_up` once they are all told, then for each change
    /// made after, until an [`Effect::Unwatch`] of the same tag ends it with
    /// [`Done::Unwatched`] or a [`Done::Failed`] does. [`Cursor::None`] starts now, its first
    /// batch naming where the journal is. Changes it cannot name are a
    /// [`Lost`] subtree, never left out: the cursor saved from a batch is never past a change
    /// not in it or an earlier one.
    Journal { dir: FileName, since: Cursor, tag: Tag },
    Unwatch { tag: Tag },
    /// Each file's [`Stamp`], in order, as one batch the driver runs as many at once as it
    /// can (io_uring: `IORING_OP_STATX`, thousands in flight). A symlink is stamped itself, not
    /// followed; a name that is not there is [`Stamp::GONE`]. Completes with
    /// [`Done::Stamped`]; it fails only as a whole, never for one name. `files` is shared, so
    /// an asker that compares the stamps against the names keeps them without a copy.
    Stamp { files: std::sync::Arc<Names>, tag: Tag },
    /// What is directly in `dir`, each name with its [`Stamp`], sorted by name; completes with
    /// [`Done::Entries`]. A `dir` that is not there, or not a directory, fails.
    Entries { dir: FileName, tag: Tag },
    /// Completes with [`Done::Flushed`] once every change the system had queued for the watch
    /// `watch` when this was asked has been told as that watch's [`Done::Changed`], before
    /// this completion: what a watcher reads after it has seen every change made before the
    /// flush. A change the system made after may be told before it or after. Fails for a
    /// watch not in flight, and where watches have no barrier (naming why); a watch that ends
    /// first fails it.
    Flush { watch: Tag, tag: Tag },
    /// Completes once `after` has passed.
    Timer { after: Duration, tag: Tag },
    /// The whole file mapped read-only into memory, shared with every other mapping of it
    /// (a pack of the store, the daemon's image). Completes with [`Done::Mapped`].
    ///
    /// The asker's contract: while any [`Mapping`] of the file lives, no effect writes,
    /// appends to or truncates that file in place; it is replaced only by renaming another
    /// file over it, as the store and the image do. An in-place write would change bytes under
    /// a live `&[u8]`, and a truncate would fault on the next read of the pages cut.
    Map { file: FileName, tag: Tag },
    /// A process started as `command` says, in its sandbox; completes with [`Done::Exited`]
    /// once it has exited and its output is read, or with [`Done::Failed`] when it could not
    /// start (no sandbox can be made here is a failure, never a run outside one).
    Spawn { command: Box<Command>, tag: Tag },
    /// The process of the spawn `tag` killed: its spawn completes, with the signal. No
    /// completion of its own; a spawn that ended already is left as it is.
    Kill { tag: Tag },
    /// Completes with [`Done::Accepted`] for each connection made to `at`, until an
    /// [`Effect::Unlisten`] of the same tag, which ends it with [`Done::Unlistened`]. In
    /// flight until it ends, as a watch is.
    Listen { at: Address, tag: Tag },
    Unlisten { tag: Tag },
    /// A connection made to `at`; completes with [`Done::Connected`].
    Connect { at: Address, tag: Tag },
    /// The bytes the peer of `conn` sent since the last receive, at most `most` and at most
    /// [`RECV_MOST`], appended to `into` after what it holds: `into` may be empty or hold bytes
    /// its asker keeps (the start of a frame), which stay as they are. Its capacity never
    /// shrinks, and grows only to `into.len() + most.min(RECV_MOST)`, so an asker that hands
    /// each [`Done::Read`]'s buffer to its next receive allocates nothing once it has grown.
    /// Completes with [`Done::Read`] of `into`, nothing appended once the peer has hung up, or
    /// with [`Done::Broken`] of it as given.
    Recv { conn: Conn, most: usize, into: Vec<u8>, tag: Tag },
    /// All of `bytes` sent on `conn`. Completes with [`Done::Sent`] of `bytes` cleared, its
    /// capacity kept for the next send, or with [`Done::Broken`] of it cleared.
    Send { conn: Conn, bytes: Vec<u8>, tag: Tag },
    /// `conn` closed; a receive in flight on it completes with nothing appended.
    Hangup { conn: Conn, tag: Tag },
}

impl Effect {
    pub fn tag(&self) -> Tag {
        match self {
            Effect::ReadAll { tag, .. }
            | Effect::Read { tag, .. }
            | Effect::ReadLink { tag, .. }
            | Effect::Append { tag, .. }
            | Effect::Sync { tag, .. }
            | Effect::Truncate { tag, .. }
            | Effect::Write { tag, .. }
            | Effect::List { tag, .. }
            | Effect::WriteAt { tag, .. }
            | Effect::Rename { tag, .. }
            | Effect::Mode { tag, .. }
            | Effect::Private { tag, .. }
            | Effect::Times { tag, .. }
            | Effect::Clone { tag, .. }
            | Effect::Link { tag, .. }
            | Effect::Stat { tag, .. }
            | Effect::Remove { tag, .. }
            | Effect::Watch { tag, .. }
            | Effect::Journal { tag, .. }
            | Effect::Unwatch { tag }
            | Effect::Stamp { tag, .. }
            | Effect::Entries { tag, .. }
            | Effect::Flush { tag, .. }
            | Effect::Timer { tag, .. }
            | Effect::Map { tag, .. }
            | Effect::Spawn { tag, .. }
            | Effect::Kill { tag }
            | Effect::Listen { tag, .. }
            | Effect::Unlisten { tag }
            | Effect::Connect { tag, .. }
            | Effect::Recv { tag, .. }
            | Effect::Send { tag, .. }
            | Effect::Hangup { tag, .. } => *tag,
        }
    }
}

/// Where [`Effect::Listen`] listens or [`Effect::Connect`] connects. A socket listened on is
/// reachable by its owner only; a TCP address listened on is for a webhook behind the VM's
/// own proxy. TLS is above this, in whoever speaks it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Address {
    /// A Unix socket under the driver's root, made with mode 0600.
    Unix(FileName),
    /// The listening Unix socket a service manager (systemd, launchd) passed the process as
    /// `fd`, bound at `at` under the driver's root. Listened on through a close-on-exec copy
    /// of the descriptor once it is a listening stream socket bound there; else the listen
    /// fails naming what it is. `fd` stays its passer's, and the socket's file the manager's:
    /// it is never removed. Listened on only: a connect to it fails (connect to
    /// [`Address::Unix`] of `at`).
    Inherited { fd: i32, at: FileName },
    Tcp(std::net::SocketAddr),
}

/// `unix:<name>`, `unix:<name> (fd <fd>)` or `tcp:<addr>`, for errors and logs.
impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Address::Unix(name) => write!(f, "unix:{name}"),
            Address::Inherited { fd, at } => write!(f, "unix:{at} (fd {fd})"),
            Address::Tcp(addr) => write!(f, "tcp:{addr}"),
        }
    }
}

/// A connection a listen accepted or a connect made, named by the driver.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Conn(pub u64);

/// A process to run. Paths outside the driver's root (a toolchain's, the C compiler's) are
/// absolute; everything the process may touch is in its [`Sandbox`].
#[derive(Clone, PartialEq, Eq, Debug)] // clone: in blaze-usercode's run `Spec`, once per spawn
pub struct Command {
    pub program: std::path::PathBuf,
    pub args: Vec<std::ffi::OsString>,
    /// The whole environment: nothing of the daemon's is inherited.
    pub env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    pub dir: std::path::PathBuf,
    pub stdin: Vec<u8>,
    pub sandbox: Sandbox,
    /// The most memory it may use, held by the system (a cgroup on Linux, a resource limit
    /// on macOS): the backstop behind admission's estimates.
    pub memory: Option<u64>,
}

/// What a spawned process may see and do. User code (build scripts, proc macros, tests) runs
/// in one always; [`Sandbox::Off`] is only for what `BLAZE_SANDBOX=off` or a documented
/// manifest opt-out says.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: in a `Command` and blaze-usercode's confine `Spec`, once per spawn
pub enum Sandbox {
    Off,
    On {
        /// Read-only, with what is under them.
        read: Vec<std::path::PathBuf>,
        /// Writable, with what is under them: an `OUT_DIR`, a temporary directory.
        write: Vec<std::path::PathBuf>,
        network: bool,
        /// Whether to record what it read ([`Traced`]).
        trace: bool,
        /// Mach global names it may look up beyond the few every sandboxed process may (a
        /// rendezvous the daemon serves, `dev.blaze.inproc.<pid>`): on macOS each is an
        /// `(allow mach-lookup (global-name ..))` of its profile, and a name the profile can't
        /// say fails the spawn. Ignored on Linux and Windows, which have no Mach names.
        services: Vec<String>,
    },
}

/// What a sandboxed process read, recorded by its sandbox: its result is keyed by exactly
/// these (docs/v2/plan/lanes/usercode.md section 6.5), so the paths are as the process named them.
#[derive(Clone, Default, PartialEq, Eq, Debug)] // clone: kept with a run's record (blaze-usercode, blaze-testsel, blaze-api), once per run
pub struct Traced {
    /// Each file it opened to read, and the BLAKE3 hash of its bytes then; `None` for a file
    /// it looked for and did not find.
    pub files: Vec<(std::path::PathBuf, Option<[u8; 32]>)>,
    /// Each directory it listed, and the hash of its sorted names then.
    pub dirs: Vec<(std::path::PathBuf, [u8; 32])>,
    /// Each environment variable it read, and its value then.
    pub env: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>,
    /// What it read that cannot be traced (the clock, randomness, the network): a result
    /// with any of these is keyed as cargo keys it.
    pub untraceable: Vec<Untraceable>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Untraceable {
    Clock,
    Random,
    Network,
    /// A file read through a descriptor the sandbox did not see opened.
    Descriptor,
}

/// How a spawned process ended.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: the simulator's programs and blaze-usercode's pool events, once per spawn
pub struct Exit {
    /// Its exit code, or `None` when a signal ended it ([`Exit::signal`]).
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Of a sandbox that traces.
    pub traced: Option<Traced>,
    /// The most memory it held, as the system measured it.
    pub peak_memory: u64,
}

/// A file mapped read-only by [`Effect::Map`]: its bytes, which stay as they are for as long
/// as any clone of it lives (the store and the image only ever replace a file by renaming
/// another over it, never write one in place).
#[derive(Clone)]
pub struct Mapping(std::sync::Arc<Held>);

/// Who keeps a [`Mapping`]'s bytes, dropped with its last clone.
enum Held {
    /// A file mapped by [`Effect::Map`].
    File(fs::Mapped),
    /// Bytes copied into blocks of sixteen-byte alignment: an image's records need their buffer
    /// that aligned (`blaze_format::ALIGN`), as a mapped file's page is.
    Copied(raw::Aligned),
}

impl Mapping {
    pub(crate) fn file(mapped: fs::Mapped) -> Mapping {
        Mapping(std::sync::Arc::new(Held::File(mapped))) // alloc: one per file mapped, at open
    }

    /// `bytes` copied, starting on a sixteen-byte boundary: a file's contents read into memory,
    /// for the simulator, a file too small to map, or a test.
    pub fn copy(bytes: &[u8]) -> Mapping {
        // alloc: the shared owner, once per mapping made from bytes
        Mapping(std::sync::Arc::new(Held::Copied(raw::Aligned::copy(bytes))))
    }

    pub fn bytes(&self) -> &[u8] {
        match &*self.0 {
            Held::File(mapped) => mapped.bytes(),
            Held::Copied(copied) => copied.bytes(),
        }
    }

    /// Whether `other` is a clone of this mapping, not just the same bytes.
    pub fn same(&self, other: &Mapping) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl PartialEq for Mapping {
    fn eq(&self, other: &Mapping) -> bool {
        self.bytes() == other.bytes()
    }
}

impl Eq for Mapping {}

impl fmt::Debug for Mapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Mapping({} bytes)", self.bytes().len())
    }
}

/// What [`Effect::Stat`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Meta {
    pub len: u64,
    pub dir: bool,
    /// Nanoseconds since the epoch of its last change, where the file system keeps it. A
    /// simulation has no clock of the world: there it is the simulated disk's count of changes,
    /// or the time an [`Effect::Times`] gave the file.
    pub modified_ns: u64,
}

/// Who may change a file besides the user the process runs as, as [`Effect::Private`] finds.
/// On Windows the system and the administrators count as the user: they may change every file
/// of the user's whatever its access list says, and a file an elevated process made is theirs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    /// The user's, and no one else may write it.
    Alone,
    /// Another user's.
    OtherUser,
    /// The user's, and someone else may write it: a group or others write bit, or on Windows
    /// an entry of its access list that lets someone else write.
    OthersWrite,
    /// Who owns it, or who may write it, could not be read: it is not known to be the user's
    /// alone.
    Unknown,
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Access::Alone => "is the user's alone",
            Access::OtherUser => "belongs to another user",
            Access::OthersWrite => "can be written to by other users",
            Access::Unknown => "has an owner or access that could not be read",
        })
    }
}

/// How an [`Effect`] went.
#[derive(PartialEq, Eq, Debug)]
#[cfg_attr(test, derive(Clone))] // clone: tests keep what they saw; a completion moves to its owner
pub enum Done {
    /// Of an [`Effect::ReadAll`] or [`Effect::Read`], the bytes read; of an [`Effect::Recv`],
    /// its buffer with the bytes received appended.
    Read(Vec<u8>),
    Appended,
    Synced,
    Truncated,
    Written,
    Listed(Vec<(FileName, Meta)>),
    Renamed,
    Moded,
    /// Of an [`Effect::Private`]: `None` when the directory and everything under it is the
    /// user's alone, else the first that is not and why.
    Private(Option<(FileName, Access)>),
    /// Of an [`Effect::Times`].
    Timed,
    Cloned,
    Linked,
    Stat(Option<Meta>),
    Removed,
    /// Files under a watched directory changed: each is named once, in no order. `lost` says
    /// the system dropped changes (its queue overflowed), so whoever watches must look at the
    /// whole directory again: the names are not all of them.
    Changed { files: Vec<FileName>, lost: bool },
    /// What an [`Effect::Journal`] heard.
    Journaled(Box<Journaled>),
    /// Of an [`Effect::Stamp`]: one per name, in its order.
    Stamped(Vec<Stamp>),
    /// Of an [`Effect::Entries`]: each name under the root, sorted.
    Entries(Vec<(FileName, Stamp)>),
    Unwatched,
    /// Of an [`Effect::Flush`]: the watch's changes queued when it was asked are all told.
    Flushed,
    Fired,
    Mapped(Mapping),
    Exited(Box<Exit>),
    Accepted { conn: Conn },
    Connected { conn: Conn },
    Unlistened,
    /// Of an [`Effect::Send`]: its bytes, cleared, their capacity kept.
    Sent(Vec<u8>),
    HungUp,
    /// An [`Effect::Send`] or [`Effect::Recv`] that failed, and its buffer back: a send's
    /// cleared, a receive's as it was given. The one buffer not handed back is a send's that
    /// io_uring's ring still held when the ring itself failed: the kernel may yet read it, so
    /// it stays where it is, abandoned, and `buffer` is empty and new.
    Broken { buffer: Vec<u8>, error: IoError },
    /// It did not happen. For an append, none of its bytes are durable unless a later sync
    /// says so; for a sync, nothing appended since the last successful one is known durable;
    /// for a write, the file is as it was.
    Failed(IoError),
}

impl Done {
    /// [`Done::Broken`] of a send's `bytes`, cleared.
    pub(crate) fn unsent(mut bytes: Vec<u8>, error: impl fmt::Display) -> Done {
        bytes.clear();
        // alloc: an error path.
        Done::Broken { buffer: bytes, error: IoError(error.to_string()) }
    }

    /// [`Done::Broken`] of a receive's buffer `into`, as it was given.
    pub(crate) fn unread(into: Vec<u8>, error: impl fmt::Display) -> Done {
        // alloc: an error path.
        Done::Broken { buffer: into, error: IoError(error.to_string()) }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct IoError(pub String);

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(PartialEq, Eq, Debug)]
#[cfg_attr(test, derive(Clone))] // clone: as `Done`'s
pub struct Completion {
    pub tag: Tag,
    pub done: Done,
}

/// Runs effects and reports their completions, in any order. Every method is called from the
/// one thread that runs the state machines. Code takes a driver as a type parameter (`impl
/// Driver`), never `dyn Driver`: production has one per target, [`Native`].
pub trait Driver {
    /// Starts every effect in `effects`, leaving it empty.
    fn submit(&mut self, effects: &mut Vec<Effect>);
    /// Moves completions into `out`, waiting up to `wait` for the first if none is ready.
    /// Returns how many it moved.
    fn poll(&mut self, out: &mut Vec<Completion>, wait: Duration) -> usize;
    /// How many effects were submitted and have not completed.
    fn in_flight(&self) -> usize;
    /// Monotonic time since the driver began.
    fn now(&self) -> Duration;
}

/// Runs `effects` and polls until every one of them has completed, handing each completion to
/// `on`, which may push more effects (submitted before the next poll). For drivers whose
/// effects all complete; a timer far ahead holds it until it fires, and a watch until it is
/// unwatched.
pub fn run_until_idle(
    driver: &mut impl Driver,
    effects: &mut Vec<Effect>,
    mut on: impl FnMut(Completion, &mut Vec<Effect>),
) {
    // Its callers are tests, examples and one-shot setup, never a hot loop.
    // alloc: once per call, until idle.
    let mut done = Vec::new();
    loop {
        driver.submit(effects);
        if driver.in_flight() == 0 {
            return;
        }
        driver.poll(&mut done, Duration::from_secs(60));
        for completion in done.drain(..) {
            on(completion, effects);
        }
    }
}

/// The one driver of this target. The build path is monomorphized over it and never picks
/// another at run time: a kernel that refuses it (io_uring blocked by a seccomp policy) is an
/// error naming it, not a slower driver. [`sim::Sim`] and [`threads::Threads`] stay for tests
/// and for the effects a native driver hands off (`threads::run`).
#[cfg(target_os = "linux")]
pub type Native = uring::Uring;
#[cfg(target_os = "macos")]
pub type Native = kqueue::Kqueue;
#[cfg(windows)]
pub type Native = iocp::Iocp;

/// This target's [`Native`] driver over `root`. `threads` sizes the pool kqueue and IOCP run
/// file calls on; io_uring's off-ring errands start their own.
#[cfg(target_os = "linux")]
pub fn native(root: &std::path::Path, _threads: usize) -> std::io::Result<Native> {
    uring::Uring::new(root)
}
#[cfg(target_os = "macos")]
pub fn native(root: &std::path::Path, threads: usize) -> std::io::Result<Native> {
    kqueue::Kqueue::new(root, threads)
}
#[cfg(windows)]
pub fn native(root: &std::path::Path, threads: usize) -> std::io::Result<Native> {
    iocp::Iocp::new(root, threads)
}

#[cfg(test)]
mod tests;

/// Counts each test thread's allocations, for the allocation-budget tests
/// (docs/v2/plan/goals/principles.md).
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
