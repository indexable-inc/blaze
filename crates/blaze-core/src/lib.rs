//! blaze's sans-IO core (docs/v2/plan.md section 3, docs/v2/plan.md).
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
//! On Windows [`iocp::Iocp`]. On WASI (the wasm32-wasip1 seed, docs/future/bootstrap.md)
//! [`threads::Threads`], which starts no thread there and makes each file call as it is
//! submitted. Each target's is its [`Native`] driver.
//!
//! Unsafe code is the kernel's and the mappings' only (docs/v2/plan/design/unsafe.md): each
//! driver of a kernel interface is one module allowed it below, as are `fs::mapped`,
//! `process::spawn` and `raw`'s single calls; the rest of the crate, the threads driver and
//! the simulation with it, is safe Rust.

#![deny(unsafe_code)]
#![feature(allocator_api)]
// `#![blaze::sans_io]` marks a sans-IO core for `blaze::sans_io_core`; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
// The seed's target (docs/future/bootstrap.md): WASI's positional reads and writes
// (`std::os::wasi::fs::FileExt`) and its random source (`std::random`), for temporary names
// where there is no process id.
#![cfg_attr(target_os = "wasi", feature(wasi_ext, random))]

pub mod busy;
#[cfg(target_os = "linux")]
pub mod cgroup;
#[allow(
    unsafe_code,
    reason = "getpriority and setpriority on the calling process, each one call behind a safe function"
)]
pub mod cores;
pub mod create_process;
pub mod decimal;
pub mod exit;
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
pub mod lock;
mod locks;
#[cfg(unix)]
#[allow(unsafe_code, reason = "sockets: descriptors owned from raw and the calls std lacks")]
mod net;
#[cfg(unix)]
mod process;
/// The children the drivers started and have not reaped, for a stall report to name
/// ([`children::unreaped`]).
#[cfg(unix)]
pub mod children {
    pub use crate::process::{State, Unreaped, Waiter, unreaped};
}
#[allow(
    unsafe_code,
    reason = "single system calls and the aligned-copy view, each behind a safe function"
)]
mod raw;
pub mod reaped;
pub mod signal;
pub mod sim;
#[allow(
    unsafe_code,
    reason = "registered memory the drivers read into and write from by address, behind safe accessors"
)]
pub mod spans;
mod sync;
mod tags;
pub mod threads;
#[cfg(target_os = "linux")]
#[allow(unsafe_code, reason = "the io_uring driver: its mapped rings, buffers and system calls")]
pub mod uring;
pub mod utf8;
#[cfg(target_os = "linux")]
#[allow(unsafe_code, reason = "the inotify driver: its system calls and the events it reads")]
mod watch;
pub mod wait;
pub mod wheel;

use std::{fmt, time::Duration};

pub use crate::journal::{
    Cursor, Journaled, Lost, Names, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG, Stamp,
};
pub use crate::spans::{DIRECT_ALIGN, Memory, Registered, Span, SpanRegion, Spans};
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
        self.0 == dir.0 || self.0.strip_prefix(&*dir.0).is_some_and(|rest| rest.starts_with('/'))
    }
}

impl fmt::Display for FileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What an [`Effect::Clone`] whose source is a symbolic link fails with, after the source's
/// name: a clone is made of a file, never through a link. Every driver says it the same, so an
/// asker can tell this refusal (the name is something other than the file it made) from a file
/// system that cannot share blocks.
pub const CLONE_THROUGH_A_LINK: &str =
    "is a symbolic link: blaze clones a file, never through a link";

/// What a failed [`Effect::Clone`] says after its source's name (and what it is, where known)
/// when the source is no regular file: a FIFO, a socket or a device, or on Windows a reparse
/// point that is no link (data deduplication, a cloud placeholder). Refused as it is opened,
/// never reported as a file system that cannot share blocks.
pub const CLONE_OF_NO_FILE: &str = "is no regular file: blaze clones a regular file";

/// What an [`Effect::Seal`] whose file is a symbolic link (on Windows, a junction too) fails
/// with, after the name: a seal is never set through a link. Every driver says it the same, so
/// an asker can tell this refusal (the name is something other than the file it wrote) from a
/// seal that could not be set.
pub const SEAL_THROUGH_A_LINK: &str = "is a symbolic link: a seal is never set through one";

/// What an [`Effect::Seal`] says after the name (and what it is) when the file is no regular
/// file: a FIFO, a socket, a device or a directory.
pub const SEAL_OF_NO_FILE: &str = "is no regular file: only a regular file is sealed";

/// What a failed [`Effect::ReadInto`] or [`Effect::WriteFrom`] says when the driver itself failed
/// with it in flight (io_uring's ring): the kernel may still write or read its span, which its
/// asker must never use again (the store's pool keeps that page from every later load).
pub const ABANDONED_SPAN: &str =
    "the span is abandoned to the kernel, which may still read or write it: never use it again";

/// The most bytes one [`Effect::Recv`] appends, whatever its `most`: a receive reserves its
/// room before the bytes come, so an asker of a long frame grows its buffer by this much a
/// receive, not by the frame's length at once.
pub const RECV_MOST: usize = 64 << 10;

/// IO a state machine asks for.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: tests' logs of what was asked (blaze-vfs's); a driver takes each by value
pub enum Effect {
    /// The whole file. A file that is not there reads as empty: a journal not yet written is
    /// one with no records.
    ReadAll {
        file: FileName,
        tag: Tag,
    },
    /// `len` bytes at `at`, or fewer at the end of the file.
    Read {
        file: FileName,
        at: u64,
        len: usize,
        tag: Tag,
    },
    /// `into.len` bytes at `at` of `file` read into the registered span `into`
    /// ([`Spans::register`]), with no copy and nothing allocated: the store's pool loading its
    /// pages (docs/v2/plan/design/store-pool.md). Completes with [`Done::ReadInto`] of how many
    /// were read, fewer than the span only at the end of the file. The span's bytes past them hold
    /// anything: a read around the page cache writes whole blocks, and the simulation scribbles
    /// them; an asker that keeps the span zeroes what it needs zeroed. A `file` that is not there
    /// fails, naming it. So does a span whose region is
    /// not registered or that runs past its region's end, and, with [`Cache::Bypassed`], an
    /// offset, address or length that is not a whole number of [`DIRECT_ALIGN`].
    ///
    /// The asker's contract: while it is in flight, nothing reads, writes or gives back the
    /// span's bytes; once it fails, they hold anything (a part may have been read), and where
    /// its failure says [`ABANDONED_SPAN`] they are never to be used again.
    ReadInto {
        file: FileName,
        at: u64,
        into: Span,
        cache: Cache,
        priority: Priority,
        tag: Tag,
    },
    /// The registered span `from` written at `at` of `file`, which exists, past its end making
    /// it longer; durable once a later [`Effect::Sync`] of the file completes. Completes with
    /// [`Done::Written`]. What [`Effect::ReadInto`] refuses of a span, this refuses too.
    ///
    /// The asker's contract: while it is in flight, nothing writes or gives back the span's
    /// bytes, which the system may read at any time until it completes. One that fails, or is in
    /// flight at a crash, may have written any of its blocks.
    WriteFrom {
        file: FileName,
        at: u64,
        from: Span,
        cache: Cache,
        tag: Tag,
    },
    /// The target of the symbolic link `file`, never followed: completes with [`Done::Read`]
    /// of the target's bytes as the link holds them (on Windows UTF-8, `\` written `/`). A
    /// `file` that is not a symbolic link, or is not there, fails naming it.
    ReadLink {
        file: FileName,
        tag: Tag,
    },
    /// Bytes added at the end of the file, creating it. Not durable until a [`Effect::Sync`] of
    /// the file asked for after this one has completed.
    Append {
        file: FileName,
        bytes: Vec<u8>,
        tag: Tag,
    },
    /// Everything appended to the file before this was asked for is durable once it completes.
    Sync {
        file: FileName,
        tag: Tag,
    },
    /// The file cut to its first `len` bytes (made if missing); durable once a later sync
    /// completes.
    Truncate {
        file: FileName,
        len: u64,
        tag: Tag,
    },
    /// The file's whole contents replaced by `bytes`, durably and all at once: a reader or a
    /// crash sees the old contents or the new, never a part. Its directories are made.
    Write {
        file: FileName,
        bytes: Vec<u8>,
        tag: Tag,
    },
    /// As [`Effect::Write`], waiting for no sync: the file's whole contents replaced by `bytes`
    /// through a temporary file renamed over it, so a reader sees the old contents or the new,
    /// never a part. Nothing of it is durable: a crash leaves the old contents (or no file), the
    /// new, or a file holding any part of the new. For derived data whose reader checks it
    /// before it uses it after a crash (the store's whole files of outputs, checked against
    /// their names). Its directories are made. Completes with [`Done::Written`].
    WriteUnsynced {
        file: FileName,
        bytes: Vec<u8>,
        tag: Tag,
    },
    /// Every file and directory under `dir`, at any depth, sorted by name, each with its
    /// [`Meta`]. A symbolic link is listed as a file of its own length and not followed.
    /// Completes with [`Done::Listed`]; a `dir` that is not there fails.
    List {
        dir: FileName,
        tag: Tag,
    },
    /// `bytes` written at `at` of `file`, which exists, past its end making it longer; durable
    /// once a later [`Effect::Sync`] of the file completes. Completes with [`Done::Written`].
    /// For a link writing the pages that changed into a clone of the last executable.
    WriteAt {
        file: FileName,
        at: u64,
        bytes: Vec<u8>,
        tag: Tag,
    },
    /// `from` renamed over `to`, atomically: a reader sees the old `to` or the new, and the
    /// new name survives a crash once it completes. Completes with [`Done::Renamed`].
    Rename {
        from: FileName,
        to: FileName,
        tag: Tag,
    },
    /// As [`Effect::Rename`], waiting for no sync of either directory: a reader sees the old
    /// `to` or the new, and a crash may leave the names as they were before it. For a file
    /// whose bytes are not durable either (an output placed from the store, which a build
    /// places again). Completes with [`Done::Renamed`].
    RenameUnsynced {
        from: FileName,
        to: FileName,
        tag: Tag,
    },
    /// The file's permission bits set to `mode` (0o755 for an executable), through a symbolic
    /// link at `file` to the file it points to on Unix (`chmod`); Windows sets the link's own.
    /// Completes with [`Done::Moded`].
    Mode {
        file: FileName,
        mode: u32,
        tag: Tag,
    },
    /// As [`Effect::Mode`], of the file itself, for a store sealing what it placed: a symbolic
    /// link (on Windows any reparse point) or anything else that is no regular file at `file`
    /// is refused, naming it, never followed or changed. Set on the file opened `O_NOFOLLOW`
    /// (`fchmod`), never by `fchmodat(AT_SYMLINK_NOFOLLOW)`, which glibc makes `fchmodat2`, a
    /// call some seccomp filters refuse. Completes with [`Done::Moded`].
    Seal {
        file: FileName,
        mode: u32,
        tag: Tag,
    },
    /// `dir` made the user's alone, and checked to be with everything under it, for what keeps
    /// code it runs (the store: a value is checked against its own hash, never against what its
    /// key says made it, so whoever may write there can plant code under any key). A missing
    /// `dir` is made with its parents, on Unix with mode 0o700. Then `dir` and every file and
    /// directory under it, links followed, is checked to be the user's with no one else able
    /// to write it ([`Access`]); once `dir` is, on Unix the group's and others' bits are taken
    /// off it, so that no one else reads what was built either. What is refused is never
    /// mended. Completes with [`Done::Private`].
    Private {
        dir: FileName,
        tag: Tag,
    },
    /// The file's access and modification times both set to `modified_ns`, nanoseconds since
    /// the Unix epoch, as cargo gives a build script's `output` the time its run began. Of the
    /// file as it is when this runs, so asked once the write that made it has completed; a
    /// symbolic link's target. Durable once a later [`Effect::Sync`] of the file completes.
    /// Completes with [`Done::Timed`]. A file that is not there fails, and so does a time the
    /// system's call cannot say (past a 32-bit `time_t`), naming the file; what is kept is the
    /// file system's resolution of it (100 ns on NTFS, 1 s on HFS+).
    Times {
        file: FileName,
        modified_ns: u64,
        tag: Tag,
    },
    /// `to` made a file with `from`'s contents that shares its blocks (`FICLONE`,
    /// `fclonefileat`): a new file's mode (0o666 less the umask) and modified now, whatever
    /// `from`'s are (a store's copies are read-only). A symbolic link `from` is refused, never
    /// followed or cloned, with [`CLONE_THROUGH_A_LINK`]. Fails where the file system cannot
    /// share blocks, naming both files' volumes: nothing copies instead.
    Clone {
        from: FileName,
        to: FileName,
        tag: Tag,
    },
    /// `to` made another name of `from`, for outputs nothing writes in place.
    Link {
        from: FileName,
        to: FileName,
        tag: Tag,
    },
    /// What the file is, or that it is not there; a symbolic link is followed to what it
    /// points to.
    Stat {
        file: FileName,
        tag: Tag,
    },
    /// As [`Effect::Stat`], a symbolic link at `file` not followed: it is a file of its own
    /// length (as [`Effect::List`] lists it; on Windows a reparse point's own), so a store's
    /// first look at a file it placed sees a link put there as the wrong length.
    Lstat {
        file: FileName,
        tag: Tag,
    },
    /// The file, or an empty directory, gone. Gone already is done.
    Remove {
        file: FileName,
        tag: Tag,
    },
    /// The advisory lock of `file`, taken without waiting: completes with [`Done::Locked`] once
    /// taken, or with [`Done::Contended`] when another holder has it in a way that excludes
    /// `mode` (another process, or another lock this driver took of the same file), and then
    /// nothing is held. The file is made if missing, with its directories, and opened to read
    /// and write, never truncated. A lock taken is held until an [`Effect::Unlock`] of its tag,
    /// or until the process ends: a crash lets every lock go. On Unix it is `flock`'s, on
    /// Windows `LockFileEx`'s. A lock asked under a tag that holds one already fails, naming the
    /// tag and the file. There is no lock that waits: a waiter retries on its own timer.
    Lock {
        file: FileName,
        mode: LockMode,
        tag: Tag,
    },
    /// The lock that [`Effect::Lock`] of tag `lock` took let go of, its file closed. Completes
    /// with [`Done::Unlocked`]; a `lock` that holds nothing fails, naming it.
    Unlock {
        lock: Tag,
        tag: Tag,
    },
    /// Completes with [`Done::Changed`] each time files under `dir` change, until an
    /// [`Effect::Unwatch`] of the same tag, which ends it with [`Done::Unwatched`], or a
    /// [`Done::Failed`], which ends it too. It is in flight until it ends; an unwatch has no
    /// completion of its own but the watch's end. A change that names `dir` itself says the
    /// directory may have been replaced or moved (inotify, which watches an inode, says so of
    /// an overflow too): a watcher that must hear of every later change watches again.
    Watch {
        dir: FileName,
        tag: Tag,
    },
    /// A watch of `dir` that starts where `since` left the system's journal of changes
    /// (docs/v2/plan/design/restore.md): it completes with [`Done::Journaled`] for the changes
    /// made since then, one with `caught_up` once they are all told, then for each change
    /// made after, until an [`Effect::Unwatch`] of the same tag ends it with
    /// [`Done::Unwatched`] or a [`Done::Failed`] does. [`Cursor::None`] starts now, its first
    /// batch naming where the journal is. Changes it cannot name are a
    /// [`Lost`] subtree, never left out: the cursor saved from a batch is never past a change
    /// not in it or an earlier one.
    Journal {
        dir: FileName,
        since: Cursor,
        tag: Tag,
    },
    Unwatch {
        tag: Tag,
    },
    /// Each file's [`Stamp`], in order, as one batch the driver runs as many at once as it
    /// can (io_uring: `IORING_OP_STATX`, thousands in flight). A symlink is stamped itself, not
    /// followed; a name that is not there is [`Stamp::GONE`]. Completes with
    /// [`Done::Stamped`]; it fails only as a whole, never for one name. `files` is shared, so
    /// an asker that compares the stamps against the names keeps them without a copy.
    Stamp {
        files: std::sync::Arc<Names>,
        tag: Tag,
    },
    /// What is directly in `dir`, each name with its [`Stamp`], sorted by name; completes with
    /// [`Done::Entries`]. A `dir` that is not there, or not a directory, fails.
    Entries {
        dir: FileName,
        tag: Tag,
    },
    /// Completes with [`Done::Flushed`] once every change the system had queued for the watch
    /// `watch` when this was asked has been told as that watch's [`Done::Changed`], before
    /// this completion: what a watcher reads after it has seen every change made before the
    /// flush. A change the system made after may be told before it or after. Fails for a
    /// watch not in flight, and where watches have no barrier (naming why); a watch that ends
    /// first fails it.
    Flush {
        watch: Tag,
        tag: Tag,
    },
    /// Completes once `after` has passed.
    Timer {
        after: Duration,
        tag: Tag,
    },
    /// The whole file mapped read-only into memory, shared with every other mapping of it
    /// (a pack of the store, the daemon's image). Completes with [`Done::Mapped`].
    ///
    /// The asker's contract: while any [`Mapping`] of the file lives, no effect writes,
    /// appends to or truncates that file in place; it is replaced only by renaming another
    /// file over it, as the store and the image do. An in-place write would change bytes under
    /// a live `&[u8]`, and a truncate would fault on the next read of the pages cut.
    Map {
        file: FileName,
        tag: Tag,
    },
    /// A process started as `command` says, in its sandbox; completes with [`Done::Exited`]
    /// once it has exited and its output is read, or with [`Done::Failed`] when it could not
    /// start (no sandbox can be made here is a failure, never a run outside one).
    Spawn {
        command: Box<Command>,
        tag: Tag,
    },
    /// The process of the spawn `tag` killed: its spawn completes, with the signal. No
    /// completion of its own; a spawn that ended already is left as it is.
    Kill {
        tag: Tag,
    },
    /// Completes with [`Done::Accepted`] for each connection made to `at`, until an
    /// [`Effect::Unlisten`] of the same tag, which ends it with [`Done::Unlistened`]. In
    /// flight until it ends, as a watch is.
    Listen {
        at: Address,
        tag: Tag,
    },
    Unlisten {
        tag: Tag,
    },
    /// A connection made to `at`; completes with [`Done::Connected`].
    Connect {
        at: Address,
        tag: Tag,
    },
    /// The bytes the peer of `conn` sent since the last receive, at most `most` and at most
    /// [`RECV_MOST`], appended to `into` after what it holds: `into` may be empty or hold bytes
    /// its asker keeps (the start of a frame), which stay as they are. Its capacity never
    /// shrinks, and grows only to `into.len() + most.min(RECV_MOST)`, so an asker that hands
    /// each [`Done::Read`]'s buffer to its next receive allocates nothing once it has grown.
    /// Completes with [`Done::Read`] of `into`, nothing appended once the peer has hung up, or
    /// with [`Done::Broken`] of it as given.
    Recv {
        conn: Conn,
        most: usize,
        into: Vec<u8>,
        tag: Tag,
    },
    /// All of `bytes` sent on `conn`. Completes with [`Done::Sent`] of `bytes` cleared, its
    /// capacity kept for the next send, or with [`Done::Broken`] of it cleared.
    Send {
        conn: Conn,
        bytes: Vec<u8>,
        tag: Tag,
    },
    /// `conn` closed; a receive in flight on it completes with nothing appended.
    Hangup {
        conn: Conn,
        tag: Tag,
    },
    /// The system's memory pressure, watched: completes with [`Done::Pressed`] each time the
    /// system says memory is short (docs/memory-budget.md section 7, "Pressure"), until an
    /// [`Effect::Unwatch`] of the same tag ends it with [`Done::Unwatched`]. In flight until it
    /// ends, as a watch is. Where the system has no such source (Linux without PSI, or one
    /// this process may not arm; Windows and the other targets, not yet), it ends at once with
    /// a [`Done::Failed`] naming why: the asker runs without it.
    ///
    /// - macOS (kqueue): `EVFILT_MEMORYSTATUS` with `NOTE_MEMORYSTATUS_PRESSURE_WARN` and
    ///   `_CRITICAL`, the filter libdispatch's `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` is made of.
    /// - Linux (io_uring): a PSI trigger ([`PSI_TRIGGER`]) on the memory pressure of the
    ///   process's own cgroup where one is delegated to it, else of the whole system
    ///   (`/proc/pressure/memory`), polled through the ring for `POLLPRI`.
    Pressure {
        tag: Tag,
    },
}

/// The PSI trigger an [`Effect::Pressure`] arms on Linux: some task stalled on memory for 300
/// ms of a 2 s window, 15%. The window is 2 s, not 1, because the kernel lets a process without
/// `CAP_SYS_RESOURCE` arm only windows that are whole multiples of 2 s (Linux 6.5, "sched/psi:
/// allow unprivileged polling of N*2s period"), and the daemon runs as its user. The trailing
/// NUL is written too: the kernel ends the trigger's text at the last byte written.
pub const PSI_TRIGGER: &[u8] = b"some 300000 2000000\0";

impl Effect {
    pub fn tag(&self) -> Tag {
        match self {
            Effect::ReadAll { tag, .. }
            | Effect::Read { tag, .. }
            | Effect::ReadInto { tag, .. }
            | Effect::WriteFrom { tag, .. }
            | Effect::ReadLink { tag, .. }
            | Effect::Append { tag, .. }
            | Effect::Sync { tag, .. }
            | Effect::Truncate { tag, .. }
            | Effect::Write { tag, .. }
            | Effect::WriteUnsynced { tag, .. }
            | Effect::List { tag, .. }
            | Effect::WriteAt { tag, .. }
            | Effect::Rename { tag, .. }
            | Effect::RenameUnsynced { tag, .. }
            | Effect::Mode { tag, .. }
            | Effect::Seal { tag, .. }
            | Effect::Private { tag, .. }
            | Effect::Times { tag, .. }
            | Effect::Clone { tag, .. }
            | Effect::Link { tag, .. }
            | Effect::Stat { tag, .. }
            | Effect::Lstat { tag, .. }
            | Effect::Remove { tag, .. }
            | Effect::Lock { tag, .. }
            | Effect::Unlock { tag, .. }
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
            | Effect::Hangup { tag, .. }
            | Effect::Pressure { tag } => *tag,
        }
    }
}

/// Whether an [`Effect::ReadInto`] or [`Effect::WriteFrom`] goes through the system's page cache.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Cache {
    /// Through it, as every other effect's bytes go.
    Kept,
    /// Around it: `O_DIRECT` on Linux, `F_NOCACHE` on macOS, `FILE_FLAG_NO_BUFFERING` on
    /// Windows. For bytes the asker keeps in memory of its own (the store's pool), which the
    /// system then holds no second copy of, and on Linux does not charge to the daemon's cgroup a
    /// second time. The offset, the span's address and its length are each a whole number of
    /// [`DIRECT_ALIGN`], on every driver; a file system that cannot bypass its cache fails the
    /// effect, naming the file. Nothing goes through the cache instead.
    Bypassed,
}

/// How soon an [`Effect::ReadInto`] is wanted, against other reads of the disk.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Priority {
    /// A task waits for it.
    Demand,
    /// Nobody waits for it yet (a prefetch): read when the disk is otherwise idle. Linux's
    /// `IOPRIO_CLASS_IDLE` (honoured under `mq-deadline` and BFQ), macOS's `IOPOL_THROTTLE` on
    /// the thread that reads it. Windows has no per-read class: there it fails, naming why.
    Idle,
}

/// How [`Effect::Lock`] takes a file's lock.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LockMode {
    /// Held with any number of other shared locks, and with no exclusive one.
    Shared,
    /// Held alone.
    Exclusive,
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
    Inherited {
        fd: i32,
        at: FileName,
    },
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
/// these (docs/v2/plan.md section 6.5), so the paths are as the process named them.
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

/// What [`Effect::Stat`] or [`Effect::Lstat`] found.
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
    /// Of an [`Effect::ReadInto`]: how many bytes of its span were read, from its first.
    ReadInto {
        len: u32,
    },
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
    /// Of an [`Effect::Lock`]: the lock is held.
    Locked,
    /// Of an [`Effect::Lock`]: another holder has the lock in a way that excludes the mode
    /// asked, and nothing is held.
    Contended,
    /// Of an [`Effect::Unlock`]: the lock is let go of.
    Unlocked,
    /// Files under a watched directory changed: each is named once, in no order. `lost` says
    /// the system dropped changes (its queue overflowed), so whoever watches must look at the
    /// whole directory again: the names are not all of them.
    Changed {
        files: Vec<FileName>,
        lost: bool,
    },
    /// What an [`Effect::Journal`] heard.
    Journaled(Box<Journaled>),
    /// Of an [`Effect::Stamp`]: one per name, in its order.
    Stamped(Vec<Stamp>),
    /// Of an [`Effect::Entries`]: each name under the root, sorted.
    Entries(Vec<(FileName, Stamp)>),
    Unwatched,
    /// Of an [`Effect::Pressure`]: the system said memory is short, `critical` when it said so
    /// at its highest level (macOS's critical; PSI has one level, said as not critical). One
    /// of many the effect gives: it stays in flight.
    Pressed {
        critical: bool,
    },
    /// Of an [`Effect::Flush`]: the watch's changes queued when it was asked are all told.
    Flushed,
    Fired,
    Mapped(Mapping),
    Exited(Box<Exit>),
    Accepted {
        conn: Conn,
    },
    Connected {
        conn: Conn,
    },
    Unlistened,
    /// Of an [`Effect::Send`]: its bytes, cleared, their capacity kept.
    Sent(Vec<u8>),
    HungUp,
    /// An [`Effect::Send`] or [`Effect::Recv`] that failed, and its buffer back: a send's
    /// cleared, a receive's as it was given. The one buffer not handed back is a send's that
    /// io_uring's ring still held when the ring itself failed: the kernel may yet read it, so
    /// it stays where it is, abandoned, and `buffer` is empty and new.
    Broken {
        buffer: Vec<u8>,
        error: IoError,
    },
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
    /// Moves completions into `out`, waiting until `until` (on [`Driver::now`]'s clock) for
    /// the first if none is ready: [`wait::Deadline::PASSED`] only looks, and
    /// [`wait::Deadline::NEVER`] waits until one comes. An absolute deadline, never a length,
    /// so no layer between the caller and the kernel restarts it (rule deadline-waits).
    /// Returns how many it moved.
    fn poll(&mut self, out: &mut Vec<Completion>, until: wait::Deadline) -> usize;
    /// How many effects were submitted and have not completed.
    fn in_flight(&self) -> usize;
    /// Monotonic time since the driver began.
    fn now(&self) -> Duration;
}

/// How long [`run_until_idle`] waits, on the driver's clock, for its next completion before it
/// gives up: the bound of its loop (rule bounded-progress). A minute, as the 60-second poll it
/// replaced; on [`sim::Sim`] it is virtual time.
pub const IDLE_STALL: Duration = Duration::from_secs(60);

/// [`run_until_idle`] stopped with effects still in flight: none completed for
/// [`IDLE_STALL`] on the driver's clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stalled {
    /// Effects submitted and not completed when it stopped.
    pub in_flight: usize,
    /// How long it waited with nothing completing, on the driver's clock.
    pub waited: Duration,
}

impl fmt::Display for Stalled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "run_until_idle: no effect completed for {:?} on the driver's clock with {} still \
             in flight (rule bounded-progress); an effect that does not end by itself (a listen, \
             a watch, a receive nothing sends to, a timer past the bound) holds it: end that \
             effect, or poll the driver yourself",
            self.waited, self.in_flight
        )
    }
}

impl std::error::Error for Stalled {}

impl From<Stalled> for std::io::Error {
    fn from(stalled: Stalled) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::TimedOut, stalled)
    }
}

/// Runs `effects` and polls until every one of them has completed, handing each completion to
/// `on`, which may push more effects (submitted before the next poll). For drivers whose
/// effects all complete: one that does not (a listen, a watch, a timer due past
/// [`IDLE_STALL`]) ends it in [`Stalled`] once nothing has completed for [`IDLE_STALL`] on the
/// driver's clock, never in a wait without end.
pub fn run_until_idle(
    driver: &mut impl Driver,
    effects: &mut Vec<Effect>,
    mut on: impl FnMut(Completion, &mut Vec<Effect>),
) -> Result<(), Stalled> {
    // Its callers are tests, examples and one-shot setup, never a hot loop.
    // alloc: once per call, until idle.
    let mut done = Vec::new();
    let mut since = driver.now();
    loop {
        driver.submit(effects);
        if driver.in_flight() == 0 {
            return Ok(());
        }
        let stall = wait::Deadline::after(since, IDLE_STALL);
        if driver.poll(&mut done, stall) > 0 {
            since = driver.now();
        } else if stall.passed(driver.now()) {
            let waited = driver.now().saturating_sub(since);
            return Err(Stalled { in_flight: driver.in_flight(), waited });
        }
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
#[cfg(target_os = "wasi")]
pub type Native = threads::Threads;

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
/// `threads` is not used: WASI starts none.
#[cfg(target_os = "wasi")]
pub fn native(root: &std::path::Path, threads: usize) -> std::io::Result<Native> {
    Ok(threads::Threads::new(root, threads))
}

#[cfg(test)]
mod tests;

/// Counts each test thread's allocations, for the allocation-budget tests
/// (docs/v2/plan.md).
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
