//! Files (docs/v2/plan/design/std.md, "Capabilities" and "Files: whether bytes reached the
//! disk is in the type"): [`Fs`] gives directories, a [`Dir<P>`] gives files beneath it by
//! relative [`Path`] only, a [`File<P>`] gives positional reads and writes. `P` is
//! [`ReadOnly`] or [`ReadWrite`]; the methods that change anything are on `Dir<ReadWrite>`
//! and `File<ReadWrite>` only, and narrowing ([`Dir::subdir`], [`Dir::read_only`],
//! [`File::read_only`]) never widens.
//!
//! **Durability is said at every call, and nothing syncs unasked.** Every call that changes a
//! file or a name comes in three forms:
//!
//! | form | takes | waits for the device |
//! |---|---|---|
//! | `x_unsynced` | [`Unsynced`] (`No`, or `Started`: write-back begun, nothing waited for) | never |
//! | `x_synced` | nothing more | yes: the file's bytes, then its directory's names |
//! | `x` | [`Durable`], the policy as a value | when it is `Durable::Yes` |
//!
//! The first two exist because an effect cannot depend on a run-time value: code that must
//! not wait for the device calls `_unsynced` and cannot reach a sync. There is no default.
//!
//! **Whether a file's bytes reached the disk is its type.** A [`Written`] may hold bytes
//! still in the page cache; [`Written::flush`] is the wait and gives a [`Flushed`].
//! [`Flushed::clone_to`] exists on `Flushed` alone (a clone of a file with unflushed bytes
//! makes the kernel write them out first, hundreds of times the clone's own cost);
//! `copy_to`, a plain write of the bytes, is on both. A `Flushed` says its bytes had reached
//! the disk when it was made: a write through another handle after that makes them unflushed
//! again, which no type sees and the simulation counts
//! ([`crate::disk::DiskCounts::forced_writeback`]).
//!
//! **Say what a path should hold.** [`Dir::publish`] makes a path hold a [`Content`] (bytes,
//! a clone of a flushed file, a copy of any file, a flushed file with ranges changed),
//! replaced all at once by a rename, and says [`How`] it did it.
//!
//! **What a crash keeps**, on the simulated disk ([`crate::disk`]):
//!
//! | call | after a crash the path holds |
//! |---|---|
//! | `publish_synced` returned | exactly the content |
//! | `publish_unsynced` returned | what it held durably before, or any prefix of the content |
//! | `remove_synced` returned | nothing |
//! | `remove_unsynced` returned | nothing, or what it held |
//! | an append, a write at an offset or a cut, then `flush` returned | every byte of them |
//! | the same with no flush | a prefix of them |
//!
//! **No hidden buffer.** Every append and write is handed to the driver by the call that
//! makes it. A writer that keeps bytes back is another type, [`Buffered`], whose buffer its
//! maker sized, whose pushes say whether they kept or wrote ([`Pushed`]), and which is linear:
//! it cannot be dropped with bytes it never wrote.
//!
//! **Finishing.** A `File<ReadWrite>` is linear: it holds a [`ReadWrite`], which is
//! `#[blaze::linear]`, so leaving one is a compile error. It ends with
//! [`File::close_synced`] (a [`Flushed`]), [`File::close_unsynced`] (a [`Written`]) or
//! [`File::close`] with a [`Durable`]: closing never syncs unasked and never skips a sync
//! unasked. A panic that unwinds past one closes nothing and flushes nothing: there is no
//! handle to close (effects name files), so its abandonment path is its writes left
//! unflushed, as a crash would find them. A `File<ReadOnly>`, a `Written`, a `Flushed` and a
//! [`Dir`] of either rights are not linear: dropping one loses nothing. The positional ops in
//! flight, [`File::read_into`] and [`File::write_from`], are [`Pending`] values.
//!
//! **What blaze-core's effects decide for bstd today.**
//!
//! - Each file is a name under the driver's root and an `Io`: effects name files, not
//!   handles. So a read-only narrowing is the type's alone, not the OS's, and an open is
//!   beneath its directory by the name's form (no root, no `..`), not by the kernel's
//!   resolution: a symbolic link inside a directory is followed by the driver. Both need an
//!   open that returns a handle with rights, resolved beneath a directory.
//! - A real driver's rename syncs its directories itself, so a rename, and a publish, costs
//!   that sync whatever [`Durable`] says: `Durable::No` is honest about what is promised, not
//!   yet about what is paid. It needs a rename effect that does not sync.
//! - A directory's names are synced by an effect (`Effect::SyncDir`), which every `_synced`
//!   name change and [`Dir::sync_names`] wait for. The driver's root itself has no name an
//!   effect can carry: a name directly beneath it is [`FsKind::NoNameSync`] on a real driver.
//! - [`Unsynced::Started`] begins the file's write-back (`Effect::WriteBack`) once the
//!   change is made, and for a name change its directory's sync (`Effect::SyncDir`), and
//!   waits for neither: their ends are dropped as they arrive. A write-back makes no file, so
//!   one still in flight brings nothing back of a file removed or renamed away meanwhile.
//! - A directory is there once a file under it is (no effect makes an empty one).
//! - A read's bytes come back in a buffer the driver made (`Done::Read`), so
//!   [`File::read_all`] and `copy_to` pass through one global allocation each read; the page
//!   pool (std.md, S1f) replaces it with a view.

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::{fmt, marker::PhantomData, num::NonZero};

use blaze_core::{Done, Effect, FileName};
use blaze_mem::Frame;

use crate::{
    Plant, Stopped,
    budget::{Budget, Over},
    cx::Cx,
    disk::dir_of,
    io::{Ends, Io},
    mem::Bytes,
    path::Path,
    pending::{self, Pending},
};

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::ReadOnly {}
    impl Sealed for super::ReadWrite {}
}

/// What a holder may do: [`ReadOnly`] or [`ReadWrite`].
pub trait Rights: sealed::Sealed {
    const WRITE: bool;
}

/// Reads only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReadOnly;

/// Reads and changes. Linear (`#[blaze::linear]`), so a `File<ReadWrite>`, which holds one, is
/// finished with one of [`File::close`]'s forms (see the module's doc): neither `Copy` nor `Clone`, and made
/// only here. A `Dir<ReadWrite>` names it only in a `PhantomData`, which holds no value, so a
/// directory stays an ordinary value.
#[blaze::linear]
#[derive(PartialEq, Eq, Debug)]
pub struct ReadWrite(());

impl Rights for ReadOnly {
    const WRITE: bool = false;
}
impl Rights for ReadWrite {
    const WRITE: bool = true;
}

/// What a file or directory is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Meta {
    pub len: u64,
    pub dir: bool,
}


/// How durable a change must be when its call returns. A required argument: no file call has
/// a default (docs/v2/plan/design/std.md, "Files").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Durable {
    /// Nothing is asked of the device: derived data, lost or torn by a power failure and
    /// made again.
    No,
    /// Writeback of the bytes is begun and not waited for (`Effect::WriteBack`; for a name
    /// change, its directory's `Effect::SyncDir`). It promises nothing a crash keeps; a
    /// later flush has less to wait for, and a file whose write-back has ended is clean.
    Started,
    /// Synced: the bytes, then the name. The one that waits for the device.
    Yes,
}

/// A [`Durable`] that does not wait for the device: what the `_unsynced` form of each file
/// call takes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Unsynced {
    /// As [`Durable::No`].
    No,
    /// As [`Durable::Started`].
    Started,
}

impl Durable {
    /// The policy as the `_unsynced` forms take it; `None` for [`Durable::Yes`], which is the
    /// `_synced` form's.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn unsynced(self) -> Option<Unsynced> {
        match self {
            Durable::No => Some(Unsynced::No),
            Durable::Started => Some(Unsynced::Started),
            Durable::Yes => None,
        }
    }
}

impl From<Unsynced> for Durable {
    fn from(unsynced: Unsynced) -> Durable {
        match unsynced {
            Unsynced::No => Durable::No,
            Unsynced::Started => Durable::Started,
        }
    }
}

/// How a publish made its path hold its content: which of its paths it took, each a
/// different cost.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum How {
    /// The bytes were written.
    Written,
    /// A flushed file's blocks were shared: no bytes moved.
    Cloned,
    /// A file's bytes were read and written, a chunk at a time.
    Copied,
    /// A flushed file's blocks were shared, then the ranges that differ written.
    Derived,
}

impl How {
    /// What a publish records in its span for the path it took.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn says(self) -> &'static str {
        match self {
            How::Written => "bstd::fs publish: wrote the bytes",
            How::Cloned => "bstd::fs publish: cloned a flushed file",
            How::Copied => "bstd::fs publish: copied a file's bytes",
            How::Derived => "bstd::fs publish: cloned a flushed file and wrote the ranges that differ",
        }
    }
}

/// One range of a [`Content::patch_of`]: `bytes` in place of what the base holds at `at`,
/// past its end making the file longer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Patch<'a> {
    pub at: u64,
    pub bytes: &'a [u8],
}

/// What a path should hold ([`Dir::publish`]). Each constructor is one cost, said by its
/// argument's type: bytes are written, a [`Flushed`] is cloned, any other file is copied.
#[derive(Clone, Copy, Debug)]
pub struct Content<'a> {
    kind: Kind<'a>,
}

#[derive(Clone, Copy, Debug)]
enum Kind<'a> {
    Bytes(&'a [u8]),
    CloneOf(&'a Flushed),
    CopyOf { of: &'a File<ReadOnly>, chunk: NonZero<usize> },
    PatchOf { base: &'a Flushed, patches: &'a [Patch<'a>] },
}

impl<'a> Content<'a> {
    /// These bytes, written.
    pub const fn bytes(bytes: &'a [u8]) -> Content<'a> {
        Content { kind: Kind::Bytes(bytes) }
    }

    /// What `base` holds, by sharing its blocks: no bytes move and, `base` being flushed,
    /// none are forced out. A file system that cannot share blocks fails the publish, naming
    /// both volumes; nothing copies in its place ([`Content::copy_of`] is the copy).
    pub const fn file(base: &'a Flushed) -> Content<'a> {
        Content { kind: Kind::CloneOf(base) }
    }

    /// What `of` holds, read and written `chunk` bytes at a time: the path for a file whose
    /// bytes may not have reached the disk ([`Written::as_file`]), which a clone would force
    /// out, and for a file system that cannot share blocks. Each chunk passes through one
    /// buffer of the driver's.
    pub const fn copy_of(of: &'a File<ReadOnly>, chunk: NonZero<usize>) -> Content<'a> {
        Content { kind: Kind::CopyOf { of, chunk } }
    }

    /// What `base` holds with each of `patches` written over it, in order: a clone, then the
    /// ranges that differ.
    pub const fn patch_of(base: &'a Flushed, patches: &'a [Patch<'a>]) -> Content<'a> {
        Content { kind: Kind::PatchOf { base, patches } }
    }
}

/// What a publish made: the file at its path, typed by whether its bytes reached the disk,
/// and how it was made.
#[derive(Clone, Debug)] // clone: authority held, copied: refcounts
pub struct Published<F> {
    pub file: F,
    pub how: How,
}

/// A file whose state the [`Durable`] of its call decided at run time.
#[derive(Clone, Debug)] // clone: authority held, copied: refcounts
pub enum Placed {
    Written(Written),
    Flushed(Flushed),
}

impl Placed {
    /// The file, to read.
    pub fn as_file(&self) -> &File<ReadOnly> {
        match self {
            Placed::Written(written) => written.as_file(),
            Placed::Flushed(flushed) => flushed.as_file(),
        }
    }

    /// The file as flushed, if its call synced it.
    pub fn flushed(self) -> Option<Flushed> {
        match self {
            Placed::Flushed(flushed) => Some(flushed),
            Placed::Written(_) => None,
        }
    }
}

/// What a file call was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsOp {
    OpenDir,
    Open,
    Create,
    Stat,
    Read,
    Append,
    WriteAt,
    SetLen,
    Flush,
    Publish,
    Remove,
    Rename,
    CloneTo,
    CopyTo,
    SyncNames,
    Close,
    List,
}

impl FsOp {
    fn name(self) -> &'static str {
        match self {
            FsOp::OpenDir => "open_dir",
            FsOp::Open => "open",
            FsOp::Create => "create",
            FsOp::Stat => "stat",
            FsOp::Read => "read",
            FsOp::Append => "append",
            FsOp::WriteAt => "write_at",
            FsOp::SetLen => "set_len",
            FsOp::Flush => "flush",
            FsOp::Publish => "publish",
            FsOp::Remove => "remove",
            FsOp::Rename => "rename_replacing",
            FsOp::CloneTo => "clone_to",
            FsOp::CopyTo => "copy_to",
            FsOp::SyncNames => "sync_names",
            FsOp::Close => "close",
            FsOp::List => "list",
        }
    }
}

/// How a file call failed.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum FsKind {
    NotFound,
    /// A file was asked for and a directory is there.
    IsADirectory,
    /// A directory was asked for and a file is there.
    NotADirectory,
    /// A new file was to be made (`clone_to`, `copy_to`) and something is there: neither
    /// replaces. [`Dir::publish`] is the call that replaces.
    Exists,
    /// The wait stopped first.
    Stopped(Stopped),
    /// The directory it came from has a quota ([`Dir::quota`]) with no room for it.
    OverQuota(Over),
    /// The names directly beneath the driver's root were to be made durable, and no effect
    /// can name the root to sync it (a `FileName` names something beneath it): refused,
    /// never said durable. The change itself was made. A name in any directory beneath the
    /// root is synced ([`blaze_core::Effect::SyncDir`]).
    NoNameSync,
    /// A [`Flushed`] was to be cloned and its file was changed after the flush it stands
    /// for (by effect `changed_at` of this world; the flush saw `flushed_at`): the clone
    /// would make the kernel write the new bytes out first. Nothing was cloned.
    Stale { flushed_at: u64, changed_at: u64 },
    /// The driver failed it, saying this.
    Driver(String),
    /// The driver answered with a completion of another kind: a driver's bug.
    Answered(String),
}

/// A file call that failed: the call, the file (under the driver's root) and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct FsError {
    pub op: FsOp,
    pub path: FileName,
    pub kind: FsKind,
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (op, path) = (self.op.name(), &self.path);
        match &self.kind {
            FsKind::NotFound => write!(f, "bstd::fs {op} of {path}: not found"),
            FsKind::IsADirectory => write!(f, "bstd::fs {op} of {path}: is a directory"),
            FsKind::NotADirectory => write!(f, "bstd::fs {op} of {path}: is a file, not a directory"),
            FsKind::Exists => write!(
                f,
                "bstd::fs {op} of {path}: something is there, and {op} makes a new file, never replaces one \
                 (Dir::publish replaces)"
            ),
            FsKind::Stopped(stopped) => write!(f, "bstd::fs {op} of {path}: {stopped}"),
            FsKind::OverQuota(over) => write!(f, "bstd::fs {op} of {path}: its directory's quota: {over}"),
            FsKind::NoNameSync => write!(
                f,
                "bstd::fs {op} of {path}: its directory is the driver's root, whose names no effect can \
                 sync (a name beneath a directory of the root is synced): the change was made and is not \
                 said durable (rule no-fallback)"
            ),
            FsKind::Stale { flushed_at, changed_at } => write!(
                f,
                "bstd::fs {op} of {path}: the file was changed after it was flushed (the flush saw \
                 change {flushed_at}, and change {changed_at} came after), so a clone would wait for \
                 its writeback: nothing was cloned (flush it again, `Written::from(flushed).flush(cx)`, \
                 or copy it)"
            ),
            FsKind::Driver(said) => write!(f, "bstd::fs {op} of {path}: {said}"),
            FsKind::Answered(done) => {
                write!(f, "bstd::fs {op} of {path}: the driver answered {done} (a driver's bug)")
            }
        }
    }
}

impl core::error::Error for FsError {}

impl FsError {
    fn new(op: FsOp, path: &FileName, kind: FsKind) -> FsError {
        // clone: an error's name of its file, a refcount.
        FsError { op, path: path.clone(), kind }
    }

    /// The stop, if the wait stopped.
    pub fn stopped(&self) -> Option<&Stopped> {
        match &self.kind {
            FsKind::Stopped(stopped) => Some(stopped),
            _ => None,
        }
    }
}

/// `done`, which was not what `op` expects, as its error.
fn unexpected(op: FsOp, path: &FileName, done: Done) -> FsError {
    let kind = match done {
        // alloc: an error's message.
        Done::Failed(error) => FsKind::Driver(error.0),
        // alloc: an error's message.
        done => FsKind::Answered(format!("{done:?}")),
    };
    FsError::new(op, path, kind)
}

/// One call of `op` on `path`: the effect `effect` makes, waited for.
fn call(
    io: &Io,
    cx: &Cx,
    op: FsOp,
    path: &FileName,
    effect: impl FnOnce(blaze_core::Tag) -> Effect,
) -> Result<Done, FsError> {
    io.call(cx, effect).map_err(|stopped| FsError::new(op, path, FsKind::Stopped(stopped)))
}

/// The one place bstd waits for a file's bytes to reach the device: everything written to
/// `name` before this was asked has, once it returns.
fn flush_wait(io: &Io, cx: &Cx, op: FsOp, name: &FileName) -> Result<(), FsError> {
    // clone: the effect's name, a refcount.
    let file = name.clone();
    match call(io, cx, op, name, |tag| Effect::Sync { file, tag })? {
        Done::Synced => {
            io.waited_for_device();
            Ok(())
        }
        done => Err(unexpected(op, name, done)),
    }
}

/// A flush of `name` handed to the driver and not waited for: its end, and its failure, are
/// dropped as they arrive. Only [`Plant::FlushedBeforeFlush`] does this: a flush in flight
/// makes its file where it is missing, so nothing sound leaves one behind.
fn flush_start(io: &Io, name: &FileName) {
    // clone: the effect's name, a refcount.
    let file = name.clone();
    let tag = io.submit(|tag| Effect::Sync { file, tag });
    io.forget(tag, Ends::Once);
}

/// What an `_unsynced` call does once its change to `name`'s bytes is made:
/// [`Unsynced::No`] asks nothing of the device; [`Unsynced::Started`] begins the file's
/// write-back ([`Effect::WriteBack`]) and waits for nothing. The write-back's end, and its
/// failure (the file removed meanwhile; a write-back makes no file), are dropped as they
/// arrive: `Started` promises only that the bytes are on their way.
fn started(io: &Io, name: &FileName, unsynced: Unsynced) {
    match unsynced {
        Unsynced::No => {}
        Unsynced::Started => {
            // clone: the effect's name, a refcount.
            let file = name.clone();
            let tag = io.submit(|tag| Effect::WriteBack { file, tag });
            io.forget(tag, Ends::Once);
        }
    }
}

/// What an `_unsynced` call does once its change to a name (a rename, a remove) is made:
/// for [`Unsynced::Started`] the sync of `name`'s directory is begun ([`Effect::SyncDir`])
/// and not waited for. The names of the driver's root, which no effect can name, are left.
fn names_started(io: &Io, name: &FileName, unsynced: Unsynced) {
    match (unsynced, FileName::new(dir_of(name))) {
        (Unsynced::Started, Some(dir)) => {
            let tag = io.submit(|tag| Effect::SyncDir { dir, tag });
            io.forget(tag, Ends::Once);
        }
        (Unsynced::Started, None) | (Unsynced::No, _) => {}
    }
}

/// The names of `name`'s directory made durable and waited for, for a `_synced` call: the
/// directory's sync ([`Effect::SyncDir`]). `renamed` says the name change was a rename: a
/// real driver's rename has synced its directories itself by the time it completes, so no
/// second sync is paid there. A name directly beneath the driver's root has no directory an
/// effect can name: [`FsKind::NoNameSync`] on a real driver (the simulation's ledger is told
/// directly).
fn sync_names(io: &Io, cx: &Cx, op: FsOp, name: &FileName, renamed: bool) -> Result<(), FsError> {
    if io.plant() == Plant::DurableSkipsNameSync {
        return Ok(());
    }
    if renamed && io.rename_syncs_names() {
        return Ok(());
    }
    sync_dir(io, cx, op, name, dir_of(name))
}

/// The directory `dir`'s names synced and waited for, on behalf of the call `op` on `path`.
fn sync_dir(io: &Io, cx: &Cx, op: FsOp, path: &FileName, dir: &str) -> Result<(), FsError> {
    let Some(named) = FileName::new(dir) else {
        cx.stopped_by(io.now()).map_err(|stopped| FsError::new(op, path, FsKind::Stopped(stopped)))?;
        return match io.names_synced(dir) {
            true => Ok(()),
            false => Err(FsError::new(op, path, FsKind::NoNameSync)),
        };
    };
    match call(io, cx, op, path, |tag| Effect::SyncDir { dir: named, tag })? {
        Done::Synced => {
            io.names_synced(dir);
            Ok(())
        }
        done => Err(unexpected(op, path, done)),
    }
}

/// `bytes` added at the end of `name`, making it; not flushed.
fn append(io: &Io, cx: &Cx, op: FsOp, name: &FileName, bytes: Vec<u8>) -> Result<(), FsError> {
    // clone: the effect's name, a refcount.
    let file = name.clone();
    match call(io, cx, op, name, |tag| Effect::Append { file, bytes, tag })? {
        Done::Appended => Ok(()),
        done => Err(unexpected(op, name, done)),
    }
}

/// `name` cut or grown to `len`, made if missing; not flushed.
fn truncate(io: &Io, cx: &Cx, op: FsOp, name: &FileName, len: u64) -> Result<(), FsError> {
    // clone: the effect's name, a refcount.
    let file = name.clone();
    match call(io, cx, op, name, |tag| Effect::Truncate { file, len, tag })? {
        Done::Truncated => Ok(()),
        done => Err(unexpected(op, name, done)),
    }
}

/// `from` renamed over `to`: a reader sees the old `to` or the new.
fn rename(io: &Io, cx: &Cx, op: FsOp, from: &FileName, to: &FileName) -> Result<(), FsError> {
    // clone: the effect's names, refcounts.
    let (old, new) = (from.clone(), to.clone());
    match call(io, cx, op, from, |tag| Effect::Rename { from: old, to: new, tag })? {
        Done::Renamed => Ok(()),
        done => Err(unexpected(op, from, done)),
    }
}

/// `name` gone; gone already is done.
fn remove(io: &Io, cx: &Cx, op: FsOp, name: &FileName) -> Result<(), FsError> {
    // clone: the effect's name, a refcount.
    let file = name.clone();
    match call(io, cx, op, name, |tag| Effect::Remove { file, tag })? {
        Done::Removed => Ok(()),
        done => Err(unexpected(op, name, done)),
    }
}

/// `to`, which is not there, made sharing `from`'s blocks.
fn clone_file(io: &Io, cx: &Cx, op: FsOp, from: &FileName, to: &FileName) -> Result<(), FsError> {
    // clone: the effect's names, refcounts.
    let (old, new) = (from.clone(), to.clone());
    match call(io, cx, op, to, |tag| Effect::Clone { from: old, to: new, tag })? {
        Done::Cloned => Ok(()),
        done => Err(unexpected(op, to, done)),
    }
}

/// `to` made to hold what `of` holds, read and appended `chunk` bytes at a time, within
/// `quota`; how many bytes. Not flushed. Each read waits under `cx`, so the copy ends by its
/// deadline however long the file is or grows.
fn copy_file(
    io: &Io,
    cx: &Cx,
    quota: Option<&Budget>,
    of: &FileName,
    to: &FileName,
    chunk: NonZero<usize>,
) -> Result<u64, FsError> {
    truncate(io, cx, FsOp::CopyTo, to, 0)?;
    let mut at = 0;
    loop {
        // clone: the effect's name, a refcount.
        let file = of.clone();
        let read = match call(io, cx, FsOp::CopyTo, of, |tag| Effect::Read { file, at, len: chunk.get(), tag })? {
            Done::Read(read) => read,
            done => return Err(unexpected(FsOp::CopyTo, of, done)),
        };
        let len = read.len();
        if len > 0 {
            within(io, quota, FsOp::CopyTo, to, len as u64, || append(io, cx, FsOp::CopyTo, to, read))?;
            at += len as u64;
        }
        if len < chunk.get() {
            return Ok(at);
        }
    }
}

/// A name beside `name`, in its directory, that no other call of this world was given: a
/// publish's temporary file, `.<leaf>.<n>.bstd-new`.
fn beside(io: &Io, name: &FileName) -> FileName {
    let fresh = io.fresh();
    let joined = match name.as_str().rsplit_once('/') {
        // alloc: once per publish, its temporary file's name.
        Some((dir, leaf)) => format!("{dir}/.{leaf}.{fresh}.bstd-new"),
        // alloc: once per publish, its temporary file's name.
        None => format!(".{name}.{fresh}.bstd-new"),
    };
    FileName::new(&joined).expect("a name beside a name is a name")
}

/// What is at `name`, or `None`.
fn stat(io: &Io, cx: &Cx, op: FsOp, name: &FileName) -> Result<Option<Meta>, FsError> {
    // clone: the effect's name, a refcount.
    let file = name.clone();
    match call(io, cx, op, name, |tag| Effect::Stat { file, tag })? {
        Done::Stat(meta) => Ok(meta.map(|it| Meta { len: it.len, dir: it.dir })),
        done => Err(unexpected(op, name, done)),
    }
}

/// `write`, which hands `bytes` bytes to the driver for `name`, run within `quota` if there
/// is one: the bytes are taken from it first, and with no room for them nothing is written
/// ([`FsKind::OverQuota`]). A write that then fails keeps what it took: some of its bytes may
/// be on the disk. Under [`Plant::BudgetCheckedAfter`] the quota is looked at once the bytes
/// are written: the planted bug of the budget tests.
fn within(
    io: &Io,
    quota: Option<&Budget>,
    op: FsOp,
    name: &FileName,
    bytes: u64,
    write: impl FnOnce() -> Result<(), FsError>,
) -> Result<(), FsError> {
    let Some(quota) = quota else { return write() };
    let over = |over| FsError::new(op, name, FsKind::OverQuota(over));
    if io.plant() == Plant::BudgetCheckedAfter {
        write()?;
        return quota.take(bytes).map_err(over);
    }
    quota.take(bytes).map_err(over)?;
    write()
}

/// The driver's whole file tree, from [`Io::fs`].
#[derive(Clone)]
pub struct Fs {
    io: Io,
}

impl Fs {
    pub(crate) fn new(io: Io) -> Fs {
        Fs { io }
    }

    /// The directory `path` under the driver's root, read and write. It may not be there yet
    /// (a file made under it makes it); a file there is refused.
    pub fn open_dir(&self, cx: &Cx, path: &Path) -> Result<Dir<ReadWrite>, FsError> {
        match stat(&self.io, cx, FsOp::OpenDir, path.name())? {
            Some(Meta { dir: false, .. }) => {
                Err(FsError::new(FsOp::OpenDir, path.name(), FsKind::NotADirectory))
            }
            // clone: the directory's name, a refcount.
            Some(_) | None => Ok(Dir { io: self.io.clone(), base: path.name().clone(), quota: None, rights: PhantomData }),
        }
    }
}

/// A directory: the files beneath it, by relative path only.
pub struct Dir<#[blaze::linear] P: Rights> {
    io: Io,
    base: FileName,
    /// The bytes that may still be written through it, if it has a quota ([`Dir::quota`]).
    quota: Option<Arc<Budget>>,
    rights: PhantomData<P>,
}

impl<#[blaze::linear] P: Rights> Clone for Dir<P> {
    fn clone(&self) -> Dir<P> {
        // clone: authority held, copied: a refcount each.
        Dir { io: self.io.clone(), base: self.base.clone(), quota: self.quota.clone(), rights: PhantomData }
    }
}

impl<#[blaze::linear] P: Rights> fmt::Debug for Dir<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rights = if P::WRITE { "ReadWrite" } else { "ReadOnly" };
        write!(f, "Dir<{rights}>({})", self.base)
    }
}

impl Dir<ReadWrite> {
    /// The directory named `base` under the driver's root, read and write: for a capability
    /// that holds a directory's name as authority ([`crate::user::User::home`]).
    pub(crate) fn at(io: &Io, base: FileName) -> Dir<ReadWrite> {
        Dir { io: io.clone(), base, quota: None, rights: PhantomData }
    }

    /// The same directory with a quota: at most `bytes` bytes are written through the `Dir`
    /// this returns, through its subdirectories and through the files opened from them,
    /// counted by bstd. A write with no room left ends in [`FsKind::OverQuota`], and nothing
    /// of it is written. A quota set on a directory that has one is inside it: both count,
    /// so it never raises what is left (docs/v2/plan/design/std.md, "Budgets ride the same
    /// way").
    ///
    /// What it counts is bytes handed to the driver: an append's, a positional write's, a
    /// publish's bytes (a copy's as they are copied, a patch's ranges; a clone hands over
    /// none), and the length a [`File::set_len`] names. A publish over a file or a remove
    /// gives nothing back, since effects name files and bstd would have to stat before every
    /// change to know what it freed; and a positional write past a file's end is counted as
    /// its bytes, not the hole before them. So the files written through it by appends and
    /// publishes of bytes never hold more than the quota; bytes held on the disk are the
    /// driver's to bound, with an effect blaze-core does not have yet.
    ///
    /// The contract: the directory it gives has at most `bytes` left, whatever it is inside.
    pub fn quota(&self, bytes: u64) -> (r: Dir<ReadWrite>)
        ensures match r.quota_left() {
            Some(left) => left <= bytes,
            None => false,
        }
    {
        // clone: authority held, narrowed: a refcount each, the budget this one is inside too.
        let quota = Some(Budget::new(bytes, self.quota.clone()));
        Dir { io: self.io.clone(), base: self.base.clone(), quota, rights: PhantomData }
    }

    /// The bytes that may still be written through it; `None` with no quota.
    pub fn quota_left(&self) -> Option<u64> {
        self.quota.as_deref().map(Budget::left)
    }
}

impl<#[blaze::linear] P: Rights> Dir<P> {
    /// Where it is, under the driver's root.
    pub fn name(&self) -> &FileName {
        &self.base
    }

    /// The directory `path` beneath this one, with the same rights or fewer.
    pub fn subdir(&self, path: &Path) -> Dir<P> {
        let base = match self.io.plant() {
            // clone: the planted bug keeps the parent's name, a refcount.
            Plant::SubdirKeepsParent => self.base.clone(),
            _ => self.beneath(path),
        };
        // clone: the quota shared, a refcount.
        Dir { io: self.io.clone(), base, quota: self.quota.clone(), rights: PhantomData }
    }

    /// The same directory, read only.
    pub fn read_only(&self) -> Dir<ReadOnly> {
        // clone: authority held, narrowed: a refcount each.
        Dir { io: self.io.clone(), base: self.base.clone(), quota: self.quota.clone(), rights: PhantomData }
    }

    /// The world it is in: for a capability made from a directory ([`crate::local::Local`]).
    pub(crate) fn io(&self) -> &Io {
        &self.io
    }

    /// `path` beneath this directory, as a path under the driver's root: for a capability
    /// that names a socket beneath a directory ([`crate::local::Local`]).
    pub(crate) fn path_beneath(&self, path: &Path) -> Path {
        Path::of(self.beneath(path))
    }

    /// Every file beneath this directory, at any depth, sorted by name, in `frame`: each one's
    /// path relative to this directory, and what it is. A directory that is not there yet (no
    /// file was made under it) has none.
    pub fn list<'a>(&self, cx: &Cx, frame: &'a Frame) -> Result<Vec<(Path, Meta), &'a Frame>, FsError> {
        let mut found = Vec::new_in(frame);
        match stat(&self.io, cx, FsOp::List, &self.base)? {
            None => return Ok(found),
            Some(Meta { dir: false, .. }) => return Err(FsError::new(FsOp::List, &self.base, FsKind::NotADirectory)),
            Some(_) => {}
        }
        // clone: the effect's name, a refcount.
        let dir = self.base.clone();
        let listed = match call(&self.io, cx, FsOp::List, &self.base, |tag| Effect::List { dir, tag })? {
            Done::Listed(listed) => listed,
            done => return Err(unexpected(FsOp::List, &self.base, done)),
        };
        let start = self.base.as_str().len() + 1;
        for (name, meta) in listed.iter().filter(|(_, meta)| !meta.dir) {
            let rest = name.is_under(&self.base).then(|| name.as_str().get(start..)).flatten();
            let beneath = rest.and_then(|rest| Path::new(rest).ok());
            let Some(path) = beneath else {
                // alloc: an error's message.
                let said = format!("a listing with {name}, which is no path beneath it");
                return Err(FsError::new(FsOp::List, &self.base, FsKind::Answered(said)));
            };
            found.try_reserve(1).map_err(|_| {
                let stopped = Stopped::OutOfMemory { arena: "bstd::fs list", asked: size_of::<(Path, Meta)>() };
                FsError::new(FsOp::List, &self.base, FsKind::Stopped(stopped))
            })?;
            found.push((path, Meta { len: meta.len, dir: false }));
        }
        Ok(found)
    }

    /// The name of `path` beneath this directory.
    fn beneath(&self, path: &Path) -> FileName {
        // alloc: once per name joined.
        let joined = format!("{}/{}", self.base, path.as_str());
        FileName::new(&joined).expect("a directory's name and a path joined are a name")
    }

    /// The name `raw` would have beneath this directory if its `..` were resolved by text:
    /// the planted bug of [`Plant::LexicalParent`], for its test only.
    #[cfg(test)]
    pub(crate) fn beneath_by_text(&self, raw: &str) -> Option<FileName> {
        // alloc: a test's planted join.
        let mut parts: Vec<&str> = self.base.as_str().split('/').collect();
        for part in raw.split('/') {
            match part {
                ".." => _ = parts.pop(),
                part => parts.push(part),
            }
        }
        // alloc: a test's planted join.
        let joined = parts.join("/");
        FileName::new(&joined)
    }

    /// What is at `path`, or `None`.
    pub fn stat(&self, cx: &Cx, path: &Path) -> Result<Option<Meta>, FsError> {
        stat(&self.io, cx, FsOp::Stat, &self.beneath(path))
    }

    /// That a file, not a directory, is at `name`, for an open. A missing one is
    /// [`FsKind::NotFound`]: an open never makes a file.
    fn found(&self, cx: &Cx, name: &FileName) -> Result<(), FsError> {
        match stat(&self.io, cx, FsOp::Open, name)? {
            Some(Meta { dir: false, .. }) => Ok(()),
            Some(Meta { dir: true, .. }) => Err(FsError::new(FsOp::Open, name, FsKind::IsADirectory)),
            None => {
                if self.io.plant() == Plant::OpenCreates {
                    // clone: the planted bug's effect's name, a refcount.
                    let file = name.clone();
                    // alloc: none, an empty file.
                    call(&self.io, cx, FsOp::Open, name, |tag| Effect::Write { file, bytes: Vec::new(), tag })?;
                }
                Err(FsError::new(FsOp::Open, name, FsKind::NotFound))
            }
        }
    }

    /// The file at `path`, read only, whatever this directory's rights. A missing one is
    /// [`FsKind::NotFound`]: an open never makes a file.
    pub fn open_read(&self, cx: &Cx, path: &Path) -> Result<File<ReadOnly>, FsError> {
        let name = self.beneath(path);
        self.found(cx, &name)?;
        Ok(File { io: self.io.clone(), name, quota: None, made: false, rights: ReadOnly })
    }

    /// The whole file at `path` added to `into`; how many bytes it added.
    pub fn read_all(&self, cx: &Cx, path: &Path, into: &mut Bytes<'_>) -> Result<usize, FsError> {
        self.open_read(cx, path)?.read_all(cx, into)
    }
}

impl Dir<ReadOnly> {
    /// The file at `path`, read only. A missing one is [`FsKind::NotFound`].
    pub fn open(&self, cx: &Cx, path: &Path) -> Result<File<ReadOnly>, FsError> {
        self.open_read(cx, path)
    }
}

impl Dir<ReadWrite> {
    /// The file at `path`, read and write: finished with one of [`File::close`]'s forms. A
    /// missing one is [`FsKind::NotFound`]: an open never makes a file.
    pub fn open(&self, cx: &Cx, path: &Path) -> Result<File<ReadWrite>, FsError> {
        let name = self.beneath(path);
        self.found(cx, &name)?;
        // clone: the quota shared, a refcount.
        Ok(File { io: self.io.clone(), name, quota: self.quota.clone(), made: false, rights: ReadWrite(()) })
    }

    /// The file at `path` made empty, in place (a file there is cut to nothing, not
    /// replaced: a reader of it sees it empty), and its directories made. Nothing of it is
    /// durable, its name included, until its close says so: finished with one of
    /// [`File::close`]'s forms. To replace a file all at once, [`Dir::publish`].
    pub fn create(&self, cx: &Cx, path: &Path) -> Result<File<ReadWrite>, FsError> {
        let name = self.beneath(path);
        truncate(&self.io, cx, FsOp::Create, &name, 0)?;
        // clone: the quota shared, a refcount.
        Ok(File { io: self.io.clone(), name, quota: self.quota.clone(), made: true, rights: ReadWrite(()) })
    }

    /// That nothing is at `name`, for a call that makes a new file and never replaces one.
    fn absent(&self, cx: &Cx, op: FsOp, name: &FileName) -> Result<(), FsError> {
        match stat(&self.io, cx, op, name)? {
            None => Ok(()),
            Some(_) => Err(FsError::new(op, name, FsKind::Exists)),
        }
    }

    /// A temporary file beside `name` made to hold `content`, not flushed: its name, and how
    /// it was made. A failure leaves the temporary file where it is; the error names it.
    fn place(&self, cx: &Cx, name: &FileName, content: Content<'_>) -> Result<(FileName, How), FsError> {
        let temp = beside(&self.io, name);
        let quota = self.quota.as_deref();
        let how = match content.kind {
            Kind::Bytes(bytes) => {
                truncate(&self.io, cx, FsOp::Publish, &temp, 0)?;
                if !bytes.is_empty() {
                    within(&self.io, quota, FsOp::Publish, &temp, bytes.len() as u64, || {
                        // alloc: the effect's bytes, which blaze-core's `Effect` owns.
                        append(&self.io, cx, FsOp::Publish, &temp, bytes.to_vec())
                    })?;
                }
                How::Written
            }
            Kind::CloneOf(base) => {
                base.not_stale(FsOp::Publish)?;
                // A name left by an earlier process: a clone never replaces.
                remove(&self.io, cx, FsOp::Publish, &temp)?;
                clone_file(&self.io, cx, FsOp::Publish, base.name(), &temp)?;
                How::Cloned
            }
            Kind::CopyOf { of, chunk } => {
                copy_file(&self.io, cx, quota, of.name(), &temp, chunk)?;
                How::Copied
            }
            Kind::PatchOf { base, patches } => {
                base.not_stale(FsOp::Publish)?;
                remove(&self.io, cx, FsOp::Publish, &temp)?;
                clone_file(&self.io, cx, FsOp::Publish, base.name(), &temp)?;
                for patch in patches {
                    within(&self.io, quota, FsOp::Publish, &temp, patch.bytes.len() as u64, || {
                        // clone: the effect's name, a refcount.
                        let file = temp.clone();
                        // alloc: the effect's bytes, which blaze-core's `Effect` owns.
                        let (at, bytes) = (patch.at, patch.bytes.to_vec());
                        match call(&self.io, cx, FsOp::Publish, &temp, |tag| Effect::WriteAt { file, at, bytes, tag })? {
                            Done::Written => Ok(()),
                            done => Err(unexpected(FsOp::Publish, &temp, done)),
                        }
                    })?;
                }
                How::Derived
            }
        };
        Ok((temp, how))
    }

    /// The temporary file `temp` renamed over `name`: the moment a reader stops seeing the
    /// old contents and sees the new. Under [`Plant::PublishBeforeRename`] the rename is
    /// handed to the driver and not waited for: the planted bug of the durability tests.
    fn swap_in(&self, cx: &Cx, temp: &FileName, name: &FileName) -> Result<(), FsError> {
        if self.io.plant() == Plant::PublishBeforeRename {
            // clone: the effect's names, refcounts.
            let (from, to) = (temp.clone(), name.clone());
            let tag = self.io.submit(|tag| Effect::Rename { from, to, tag });
            self.io.forget(tag, Ends::Once);
            return Ok(());
        }
        rename(&self.io, cx, FsOp::Publish, temp, name)
    }

    /// `path` made to hold `content`, replaced all at once, as durable as `durable` says:
    /// [`Dir::publish_unsynced`] or [`Dir::publish_synced`], chosen at run time. It waits for
    /// the device exactly when `durable` is [`Durable::Yes`]; code that must never wait for
    /// the device calls `publish_unsynced`.
    pub fn publish(
        &self,
        cx: &Cx,
        path: &Path,
        content: Content<'_>,
        durable: Durable,
    ) -> Result<Published<Placed>, FsError> {
        match durable.unsynced() {
            Some(unsynced) => {
                let Published { file, how } = self.publish_unsynced(cx, path, content, unsynced)?;
                Ok(Published { file: Placed::Written(file), how })
            }
            None => {
                let Published { file, how } = self.publish_synced(cx, path, content)?;
                Ok(Published { file: Placed::Flushed(file), how })
            }
        }
    }

    /// `path` made to hold `content`, replaced all at once for a reader (the old contents or
    /// the new, never a part), with nothing synced: it never waits for the device. After a
    /// crash the path holds what it held durably before, or any prefix of `content`: derived
    /// data, checked by its reader or made again. The path it took is in what it returns and
    /// recorded in `cx`'s span ([`How::says`]).
    pub fn publish_unsynced(
        &self,
        cx: &Cx,
        path: &Path,
        content: Content<'_>,
        unsynced: Unsynced,
    ) -> Result<Published<Written>, FsError> {
        let name = self.beneath(path);
        let (temp, how) = self.place(cx, &name, content)?;
        self.swap_in(cx, &temp, &name)?;
        started(&self.io, &name, unsynced);
        names_started(&self.io, &name, unsynced);
        cx.event(how.says());
        Ok(Published { file: Written::of(&self.io, name), how })
    }

    /// `path` made to hold `content`, replaced all at once for a reader and for a crash:
    /// once this returns a crash keeps exactly `content`; before, the old contents or the
    /// new, never a part. It waits for the device twice: the new file's bytes are flushed
    /// before it is renamed over `path`, and the directory's names synced after.
    pub fn publish_synced(&self, cx: &Cx, path: &Path, content: Content<'_>) -> Result<Published<Flushed>, FsError> {
        let name = self.beneath(path);
        let (temp, how) = self.place(cx, &name, content)?;
        let flushed = Written::of(&self.io, temp).flush(cx)?;
        self.swap_in(cx, flushed.name(), &name)?;
        sync_names(&self.io, cx, FsOp::Publish, &name, true)?;
        cx.event(how.says());
        // The name's generation is its rename's: the bytes were flushed as the temporary file,
        // and nothing changed them since.
        let seen = self.io.generation(&name);
        Ok(Published { file: Flushed::of(&self.io, name, seen), how })
    }

    /// The file at `path` gone, as durably as `durable` says; gone already is done.
    /// [`Dir::remove_unsynced`] or [`Dir::remove_synced`], chosen at run time.
    pub fn remove(&self, cx: &Cx, path: &Path, durable: Durable) -> Result<(), FsError> {
        match durable.unsynced() {
            Some(unsynced) => self.remove_unsynced(cx, path, unsynced),
            None => self.remove_synced(cx, path),
        }
    }

    /// The file at `path` gone, with nothing synced: a crash may bring it back. Gone already
    /// is done.
    pub fn remove_unsynced(&self, cx: &Cx, path: &Path, unsynced: Unsynced) -> Result<(), FsError> {
        let name = self.beneath(path);
        remove(&self.io, cx, FsOp::Remove, &name)?;
        names_started(&self.io, &name, unsynced);
        Ok(())
    }

    /// The file at `path` gone, and its directory's names synced: a crash keeps it gone.
    pub fn remove_synced(&self, cx: &Cx, path: &Path) -> Result<(), FsError> {
        let name = self.beneath(path);
        remove(&self.io, cx, FsOp::Remove, &name)?;
        sync_names(&self.io, cx, FsOp::Remove, &name, false)
    }

    /// `from` renamed over `to`, both beneath this directory, replacing what `to` held all
    /// at once for a reader, as durably as `durable` says:
    /// [`Dir::rename_replacing_unsynced`] or [`Dir::rename_replacing_synced`], chosen at run
    /// time.
    pub fn rename_replacing(&self, cx: &Cx, from: &Path, to: &Path, durable: Durable) -> Result<(), FsError> {
        match durable.unsynced() {
            Some(unsynced) => self.rename_replacing_unsynced(cx, from, to, unsynced),
            None => self.rename_replacing_synced(cx, from, to),
        }
    }

    /// `from` renamed over `to`, replacing it all at once for a reader, with nothing synced:
    /// after a crash `to` may hold what it held before, with `from` back, and the file's
    /// unflushed bytes may be torn.
    pub fn rename_replacing_unsynced(
        &self,
        cx: &Cx,
        from: &Path,
        to: &Path,
        unsynced: Unsynced,
    ) -> Result<(), FsError> {
        let (from, to) = (self.beneath(from), self.beneath(to));
        rename(&self.io, cx, FsOp::Rename, &from, &to)?;
        started(&self.io, &to, unsynced);
        names_started(&self.io, &to, unsynced);
        Ok(())
    }

    /// `from` renamed over `to`, replacing it all at once for a reader, then the file's
    /// bytes flushed and the directory's names synced: once this returns a crash keeps the
    /// file at `to`, whole. A crash before it returns may keep the new name with bytes not
    /// yet flushed: where that matters, flush the file first ([`Written::flush`]) or publish
    /// it ([`Dir::publish_synced`]).
    pub fn rename_replacing_synced(&self, cx: &Cx, from: &Path, to: &Path) -> Result<(), FsError> {
        let (from, to) = (self.beneath(from), self.beneath(to));
        rename(&self.io, cx, FsOp::Rename, &from, &to)?;
        flush_wait(&self.io, cx, FsOp::Rename, &to)?;
        sync_names(&self.io, cx, FsOp::Rename, &to, true)
    }

    /// The names directly in this directory made durable: every file made in it, renamed
    /// into it or removed from it by a call that has returned stays so through a crash. It
    /// waits for the device ([`blaze_core::Effect::SyncDir`]). A file's bytes are its own
    /// flush. The driver's root itself, which no effect can name, is
    /// [`FsKind::NoNameSync`] on a real driver.
    pub fn sync_names(&self, cx: &Cx) -> Result<(), FsError> {
        if self.io.plant() == Plant::DurableSkipsNameSync {
            return Ok(());
        }
        sync_dir(&self.io, cx, FsOp::SyncNames, &self.base, self.base.as_str())
    }
}

/// A file whose bytes may not have reached the disk: what an `_unsynced` call gives, and
/// what any file is until it is flushed. It reads ([`Written::as_file`]) and copies; it has
/// no `clone_to`, since a clone would make the kernel write its bytes out first.
/// [`Written::flush`] is the wait that makes it a [`Flushed`]. Not linear: the call that made
/// it was told nothing need be synced.
#[derive(Clone, Debug)] // clone: authority held, copied: refcounts
pub struct Written {
    file: File<ReadOnly>,
}

/// A file whose bytes had all reached the disk when this was made: what a `_synced` call and
/// [`Written::flush`] give. The one type with [`Flushed::clone_to`]. It carries the file's
/// generation as its flush saw it (the number of the last effect of this world that changed
/// the file), and a clone of a file changed since is refused ([`FsKind::Stale`]): a
/// `Flushed` does not go stale silently. A change by another process is not seen (blaze-core
/// reports no inode or change time yet); the simulation still counts what any clone forces
/// out ([`crate::disk::DiskCounts::forced_writeback`]).
#[derive(Clone, Debug)] // clone: authority held, copied: refcounts
pub struct Flushed {
    file: File<ReadOnly>,
    /// The file's generation ([`Io::generation`]) read before the flush this stands for.
    seen: u64,
}

impl From<File<ReadOnly>> for Written {
    /// Any file may be taken as one whose bytes may not have reached the disk: it claims
    /// less.
    fn from(file: File<ReadOnly>) -> Written {
        Written { file }
    }
}

impl From<Flushed> for Written {
    /// A flushed file taken as one that may not be: it claims less. The way back from a
    /// stale `Flushed` is this and [`Written::flush`].
    fn from(flushed: Flushed) -> Written {
        Written { file: flushed.file }
    }
}

impl Written {
    fn of(io: &Io, name: FileName) -> Written {
        Written { file: File { io: io.clone(), name, quota: None, made: false, rights: ReadOnly } }
    }

    /// The file, to read.
    pub fn as_file(&self) -> &File<ReadOnly> {
        &self.file
    }

    pub fn name(&self) -> &FileName {
        &self.file.name
    }

    /// The wait for the device: every byte written to the file before this was called has
    /// reached the disk once it returns, and the file is [`Flushed`]. Its bytes only: its
    /// name is durable once its directory's names are synced ([`Dir::sync_names`], or the
    /// `_synced` call that named it).
    pub fn flush(self, cx: &Cx) -> Result<Flushed, FsError> {
        let Written { file } = self;
        // Read before the flush is asked for: a change that lands while it runs is one the
        // flush may not cover, and makes the `Flushed` stale.
        let seen = file.io.generation(&file.name);
        if file.io.plant() == Plant::FlushedBeforeFlush {
            flush_start(&file.io, &file.name);
            return Ok(Flushed { file, seen });
        }
        flush_wait(&file.io, cx, FsOp::Flush, &file.name)?;
        Ok(Flushed { file, seen })
    }

    /// A new file at `path` beneath `dir` holding what this holds, read and written `chunk`
    /// bytes at a time: a copy, proportional to the file. It never replaces: something at
    /// `path` is [`FsKind::Exists`] (checked first, so a file made there by another task in
    /// between is appended over, not refused; [`Dir::publish`] with [`Content::copy_of`] is
    /// the form that replaces all at once). The new file is not flushed and its name not
    /// synced.
    pub fn copy_to(
        &self,
        cx: &Cx,
        dir: &Dir<ReadWrite>,
        path: &Path,
        chunk: NonZero<usize>,
    ) -> Result<Written, FsError> {
        copy_to(&self.file, cx, dir, path, chunk)
    }
}

impl Flushed {
    fn of(io: &Io, name: FileName, seen: u64) -> Flushed {
        Flushed { file: File { io: io.clone(), name, quota: None, made: false, rights: ReadOnly }, seen }
    }

    /// Whether the file was changed, through this world, after the flush this stands for:
    /// then [`Flushed::clone_to`] and a publish that clones it refuse it.
    pub fn is_stale(&self) -> bool {
        self.file.io.generation(&self.file.name) != self.seen
    }

    /// What every clone of this file checks first, for the call `op`: [`FsKind::Stale`],
    /// naming the file, when it was changed after its flush. Under
    /// [`Plant::StaleFlushedCloned`] nothing is checked: the planted bug of the stale-clone
    /// test.
    fn not_stale(&self, op: FsOp) -> Result<(), FsError> {
        let changed_at = self.file.io.generation(&self.file.name);
        if changed_at == self.seen || self.file.io.plant() == Plant::StaleFlushedCloned {
            return Ok(());
        }
        Err(FsError::new(op, &self.file.name, FsKind::Stale { flushed_at: self.seen, changed_at }))
    }

    /// The file, to read.
    pub fn as_file(&self) -> &File<ReadOnly> {
        &self.file
    }

    pub fn name(&self) -> &FileName {
        &self.file.name
    }

    /// A new file at `path` beneath `dir` sharing this one's blocks: no bytes move, and none
    /// are forced out, this file being flushed. It never replaces: something at `path` is
    /// [`FsKind::Exists`]. A file system that cannot share blocks fails it, naming both
    /// volumes; nothing copies in its place ([`Flushed::copy_to`] is the copy). The new
    /// file's name is not synced. A file changed since its flush is refused before anything
    /// is made ([`FsKind::Stale`], naming the file): flushing it again gives a `Flushed` that
    /// clones.
    pub fn clone_to(&self, cx: &Cx, dir: &Dir<ReadWrite>, path: &Path) -> Result<Written, FsError> {
        self.not_stale(FsOp::CloneTo)?;
        let to = dir.beneath(path);
        dir.absent(cx, FsOp::CloneTo, &to)?;
        clone_file(&dir.io, cx, FsOp::CloneTo, &self.file.name, &to)?;
        Ok(Written::of(&dir.io, to))
    }

    /// As [`Written::copy_to`]: a copy, where a clone is not wanted or cannot be made.
    pub fn copy_to(
        &self,
        cx: &Cx,
        dir: &Dir<ReadWrite>,
        path: &Path,
        chunk: NonZero<usize>,
    ) -> Result<Written, FsError> {
        copy_to(&self.file, cx, dir, path, chunk)
    }
}

/// [`Written::copy_to`] and [`Flushed::copy_to`].
fn copy_to(
    of: &File<ReadOnly>,
    cx: &Cx,
    dir: &Dir<ReadWrite>,
    path: &Path,
    chunk: NonZero<usize>,
) -> Result<Written, FsError> {
    let to = dir.beneath(path);
    dir.absent(cx, FsOp::CopyTo, &to)?;
    copy_file(&dir.io, cx, dir.quota.as_deref(), &of.name, &to, chunk)?;
    Ok(Written::of(&dir.io, to))
}

/// A file beneath a directory. A `File<ReadWrite>` is linear, finished with one of
/// [`File::close`]'s forms;
/// a `File<ReadOnly>` is not (see the module's doc).
pub struct File<#[blaze::linear] P: Rights> {
    io: Io,
    name: FileName,
    /// The quota of the directory it was opened from, which its writes count against; none
    /// on a read-only file, which writes nothing.
    quota: Option<Arc<Budget>>,
    /// Whether this handle made the file ([`Dir::create`]): then its name is no more durable
    /// than its directory's last name sync, and [`File::close_synced`] syncs it.
    made: bool,
    /// The rights themselves, not a `PhantomData`: a [`ReadWrite`] held makes the file linear.
    rights: P,
}

impl Clone for File<ReadOnly> {
    fn clone(&self) -> File<ReadOnly> {
        // clone: authority held, copied: a refcount each.
        File { io: self.io.clone(), name: self.name.clone(), quota: None, made: false, rights: ReadOnly }
    }
}

impl<#[blaze::linear] P: Rights> fmt::Debug for File<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rights = if P::WRITE { "ReadWrite" } else { "ReadOnly" };
        write!(f, "File<{rights}>({})", self.name)
    }
}

impl<#[blaze::linear] P: Rights> File<P> {
    pub fn name(&self) -> &FileName {
        &self.name
    }

    /// The same file, read only.
    pub fn read_only(&self) -> File<ReadOnly> {
        // clone: authority held, narrowed: a refcount each.
        File { io: self.io.clone(), name: self.name.clone(), quota: None, made: false, rights: ReadOnly }
    }

    /// Its length now.
    pub fn len(&self, cx: &Cx) -> Result<u64, FsError> {
        match stat(&self.io, cx, FsOp::Stat, &self.name)? {
            Some(meta) => Ok(meta.len),
            None => Err(FsError::new(FsOp::Stat, &self.name, FsKind::NotFound)),
        }
    }

    /// A read of up to `into.len()` bytes at `at` into `into`, in flight: its outcome, once
    /// waited for, is how many bytes it read, fewer at the end of the file.
    pub fn read_into<'buf>(&'buf self, into: &'buf mut [u8], at: u64) -> Pending<'buf, Result<usize, FsError>> {
        // clone: the effect's name, a refcount.
        let file = self.name.clone();
        let len = into.len();
        let name = &self.name;
        let effect = |tag| Effect::Read { file, at, len, tag };
        pending::start(&self.io, "bstd::fs read_into", into, effect, move |into: &mut [u8], done| {
            match done {
                Ok(Done::Read(bytes)) if bytes.len() <= into.len() => {
                    into[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                Ok(done) => Err(unexpected(FsOp::Read, name, done)),
                Err(stopped) => Err(FsError::new(FsOp::Read, name, FsKind::Stopped(stopped))),
            }
        })
    }

    /// Up to `len` bytes at `at` added to `into`, fewer at the end of the file; how many. Its
    /// [`File::read_into`], waited for.
    pub fn read_at(&self, cx: &Cx, at: u64, len: usize, into: &mut Bytes<'_>) -> Result<usize, FsError> {
        let stopped = |stopped| FsError::new(FsOp::Read, &self.name, FsKind::Stopped(stopped));
        cx.stopped_by(self.io.now()).map_err(stopped)?;
        let start = into.len();
        let slot = into.grow_zeroed(len).map_err(stopped)?;
        let read = self.read_into(slot, at).wait(cx);
        match read {
            Ok(read) => {
                into.truncate(start + read);
                Ok(read)
            }
            Err(error) => {
                into.truncate(start);
                Err(error)
            }
        }
    }

    /// The whole file added to `into`; how many bytes.
    pub fn read_all(&self, cx: &Cx, into: &mut Bytes<'_>) -> Result<usize, FsError> {
        // clone: the effect's name, a refcount.
        let file = self.name.clone();
        let done = call(&self.io, cx, FsOp::Read, &self.name, |tag| Effect::ReadAll { file, tag })?;
        match done {
            Done::Read(bytes) => {
                into.try_extend(&bytes)
                    .map_err(|stopped| FsError::new(FsOp::Read, &self.name, FsKind::Stopped(stopped)))?;
                Ok(bytes.len())
            }
            done => Err(unexpected(FsOp::Read, &self.name, done)),
        }
    }
}

impl File<ReadWrite> {
    /// The bytes handed to the driver at the file's end, within the quota.
    fn append_bytes(&self, cx: &Cx, bytes: &[u8]) -> Result<(), FsError> {
        within(&self.io, self.quota.as_deref(), FsOp::Append, &self.name, bytes.len() as u64, || {
            // alloc: the effect's bytes, which blaze-core's `Effect` owns.
            append(&self.io, cx, FsOp::Append, &self.name, bytes.to_vec())
        })
    }

    /// `bytes` added at the end, as durably as `durable` says: [`File::append_unsynced`] or
    /// [`File::append_synced`], chosen at run time.
    pub fn append(&self, cx: &Cx, bytes: &[u8], durable: Durable) -> Result<(), FsError> {
        match durable.unsynced() {
            Some(unsynced) => self.append_unsynced(cx, bytes, unsynced),
            None => self.append_synced(cx, bytes),
        }
    }

    /// `bytes` added at the end, not flushed: it never waits for the device, and a crash
    /// keeps a prefix of them until a later [`File::flush`] has returned.
    pub fn append_unsynced(&self, cx: &Cx, bytes: &[u8], unsynced: Unsynced) -> Result<(), FsError> {
        self.append_bytes(cx, bytes)?;
        started(&self.io, &self.name, unsynced);
        Ok(())
    }

    /// `bytes` added at the end, then the file flushed: once this returns a crash keeps
    /// them, and everything written to the file before them.
    pub fn append_synced(&self, cx: &Cx, bytes: &[u8]) -> Result<(), FsError> {
        self.append_bytes(cx, bytes)?;
        self.flush(cx)
    }

    /// A write of `from` at `at`, past the end making it longer, in flight; not flushed. The
    /// bytes are copied as it starts (see [`crate::pending`]), and stay lent until it ends.
    pub fn write_from<'buf>(&'buf self, from: &'buf [u8], at: u64) -> Pending<'buf, Result<(), FsError>> {
        let name = &self.name;
        let asked = from.len() as u64;
        let over = move |over| FsError::new(FsOp::WriteAt, name, FsKind::OverQuota(over));
        // The planted bug of the budget tests: the quota is looked at once the bytes are written.
        let after = self.io.plant() == Plant::BudgetCheckedAfter;
        let quota = self.quota.as_deref();
        let short = match quota {
            Some(quota) if !after => quota.take(asked).err(),
            _ => None,
        };
        if let Some(short) = short {
            return pending::refused(&self.io, "bstd::fs write_from", Err(over(short)));
        }
        // clone: the effect's name, a refcount.
        let file = self.name.clone();
        // alloc: the effect's bytes, which blaze-core's `Effect` owns.
        let bytes = from.to_vec();
        let effect = |tag| Effect::WriteAt { file, at, bytes, tag };
        pending::start(&self.io, "bstd::fs write_from", &mut [], effect, move |_: &mut [u8], done| {
            match done {
                Ok(Done::Written) => match quota {
                    Some(quota) if after => quota.take(asked).map_err(over),
                    _ => Ok(()),
                },
                Ok(done) => Err(unexpected(FsOp::WriteAt, name, done)),
                Err(stopped) => Err(FsError::new(FsOp::WriteAt, name, FsKind::Stopped(stopped))),
            }
        })
    }

    /// `bytes` written at `at`, past the end making it longer, as durably as `durable`
    /// says: [`File::write_at_unsynced`] or [`File::write_at_synced`], chosen at run time.
    pub fn write_at(&self, cx: &Cx, at: u64, bytes: &[u8], durable: Durable) -> Result<(), FsError> {
        match durable.unsynced() {
            Some(unsynced) => self.write_at_unsynced(cx, at, bytes, unsynced),
            None => self.write_at_synced(cx, at, bytes),
        }
    }

    /// `bytes` written at `at`, not flushed: its [`File::write_from`], waited for. It never
    /// waits for the device.
    pub fn write_at_unsynced(&self, cx: &Cx, at: u64, bytes: &[u8], unsynced: Unsynced) -> Result<(), FsError> {
        cx.stopped_by(self.io.now())
            .map_err(|stopped| FsError::new(FsOp::WriteAt, &self.name, FsKind::Stopped(stopped)))?;
        self.write_from(bytes, at).wait(cx)?;
        started(&self.io, &self.name, unsynced);
        Ok(())
    }

    /// `bytes` written at `at`, then the file flushed: once this returns a crash keeps them.
    pub fn write_at_synced(&self, cx: &Cx, at: u64, bytes: &[u8]) -> Result<(), FsError> {
        self.write_at_unsynced(cx, at, bytes, Unsynced::No)?;
        self.flush(cx)
    }

    /// The file cut or grown to `len`, as durably as `durable` says: [`File::set_len_unsynced`]
    /// or [`File::set_len_synced`], chosen at run time.
    pub fn set_len(&self, cx: &Cx, len: u64, durable: Durable) -> Result<(), FsError> {
        match durable.unsynced() {
            Some(unsynced) => self.set_len_unsynced(cx, len, unsynced),
            None => self.set_len_synced(cx, len),
        }
    }

    /// The file cut or grown to `len`, not flushed: it never waits for the device.
    pub fn set_len_unsynced(&self, cx: &Cx, len: u64, unsynced: Unsynced) -> Result<(), FsError> {
        within(&self.io, self.quota.as_deref(), FsOp::SetLen, &self.name, len, || {
            truncate(&self.io, cx, FsOp::SetLen, &self.name, len)
        })?;
        started(&self.io, &self.name, unsynced);
        Ok(())
    }

    /// The file cut or grown to `len`, then flushed: once this returns a crash keeps it so.
    pub fn set_len_synced(&self, cx: &Cx, len: u64) -> Result<(), FsError> {
        self.set_len_unsynced(cx, len, Unsynced::No)?;
        self.flush(cx)
    }

    /// The wait for the device: every byte written to the file before this was called has
    /// reached the disk once it returns. Its bytes only: the name of a file this handle made
    /// is durable once its directory's names are synced ([`File::close_synced`],
    /// [`Dir::sync_names`]).
    pub fn flush(&self, cx: &Cx) -> Result<(), FsError> {
        if self.io.plant() == Plant::SyncSkipped {
            return Ok(());
        }
        flush_wait(&self.io, cx, FsOp::Flush, &self.name)
    }

    /// The file finished, as durably as `durable` says: [`File::close_unsynced`] or
    /// [`File::close_synced`], chosen at run time. One of the three is the one way a
    /// `File<ReadWrite>` ends.
    #[blaze::consumes]
    pub fn close(self, cx: &Cx, durable: Durable) -> Result<Placed, FsError> {
        match durable.unsynced() {
            Some(unsynced) => self.close_unsynced(unsynced).map(Placed::Written),
            None => self.close_synced(cx).map(Placed::Flushed),
        }
    }

    /// The file finished with nothing synced: a [`Written`], whose bytes a crash may lose or
    /// tear. It takes no `Cx`: it cannot wait. Told [`Unsynced::Started`] it begins the
    /// file's write-back and waits for nothing. It cannot fail today; the `Result` is the
    /// shape every close has.
    #[blaze::consumes]
    pub fn close_unsynced(self, unsynced: Unsynced) -> Result<Written, FsError> {
        let File { io, name, quota: _, made: _, rights } = self;
        crate::consume(rights);
        started(&io, &name, unsynced);
        Ok(Written::of(&io, name))
    }

    /// The file finished and synced: its bytes flushed and, if this handle made it
    /// ([`Dir::create`]), its directory's names synced, so a crash keeps the file and every
    /// byte written to it. A [`Flushed`], or the sync's error, op [`FsOp::Close`].
    #[blaze::consumes]
    pub fn close_synced(self, cx: &Cx) -> Result<Flushed, FsError> {
        let File { io, name, quota: _, made, rights } = self;
        crate::consume(rights);
        let seen = io.generation(&name);
        if io.plant() == Plant::CloseSkipsSync {
            return Ok(Flushed::of(&io, name, seen));
        }
        flush_wait(&io, cx, FsOp::Close, &name)?;
        if made {
            sync_names(&io, cx, FsOp::Close, &name, false)?;
        }
        Ok(Flushed::of(&io, name, seen))
    }
}

/// What a [`Buffered::push`] did with its bytes: which of its two costs it paid.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pushed {
    /// Kept in the buffer: nothing was handed to the driver and nothing waited.
    Held,
    /// The buffer had no room: this many bytes (what was held, and the push's own where they
    /// are as long as the buffer) were appended to the file, each append waited for.
    Wrote(usize),
}

/// A buffer of appends to one file: bytes pushed are kept until the buffer is full, written
/// out ([`Buffered::write_out`]) or finished. A writer that buffers is this type and no
/// other: [`File::append`] hands every call to the driver and keeps nothing back.
///
/// The buffer is `capacity` bytes in the frame its maker hands in, reserved once, so a push
/// never allocates. What it writes is appended unsynced: how durable the file is stays its
/// close's to say.
///
/// Linear (`#[blaze::linear]`): bytes still held are not in the file, so leaving one
/// unfinished is a compile error, and it ends with [`Buffered::finish`], which writes them
/// out and returns that write's error. A panic that unwinds past one drops the bytes it
/// held: its abandonment path is their loss, as a crash would lose them.
#[blaze::linear]
#[must_use = "a buffered writer is finished with finish(cx): the bytes it holds are not in its file"]
pub struct Buffered<'f, 'a> {
    file: &'f File<ReadWrite>,
    held: Bytes<'a>,
    capacity: usize,
}

impl File<ReadWrite> {
    /// A buffer of appends to this file, `capacity` bytes in `frame`, reserved here: finished
    /// with [`Buffered::finish`]. With no room for it in `frame` it is
    /// [`Stopped::OutOfMemory`], naming the buffer.
    pub fn buffered<'f, 'a>(
        &'f self,
        frame: &'a Frame,
        capacity: NonZero<usize>,
    ) -> Result<Buffered<'f, 'a>, FsError> {
        let mut held = Bytes::new_in(frame, "bstd::fs buffered");
        held.try_reserve(capacity.get())
            .map_err(|stopped| FsError::new(FsOp::Append, &self.name, FsKind::Stopped(stopped)))?;
        Ok(Buffered { file: self, held, capacity: capacity.get() })
    }
}

impl Buffered<'_, '_> {
    /// How many bytes are held and not yet in the file.
    #[blaze::noalloc(trusted = "reads the buffer's length; the `dyn Probe` the file's world holds in its lock is never called")]
    #[blaze::nonblocking(trusted = "reads the buffer's length; the `dyn Probe` the file's world holds in its lock is never called")]
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// `bytes` added, in order after everything pushed before. With room in the buffer they
    /// are kept ([`Pushed::Held`]: no wait, no allocation); without, what is held is
    /// appended to the file first, and `bytes` too where they are as long as the buffer
    /// ([`Pushed::Wrote`]). On an error nothing of this push was kept or written, and what
    /// was held may have been written.
    pub fn push(&mut self, cx: &Cx, bytes: &[u8]) -> Result<Pushed, FsError> {
        if self.held.len() + bytes.len() <= self.capacity {
            self.keep(bytes)?;
            return Ok(Pushed::Held);
        }
        let mut wrote = self.write_out(cx)?;
        if bytes.len() >= self.capacity {
            self.file.append_unsynced(cx, bytes, Unsynced::No)?;
            wrote += bytes.len();
        } else {
            self.keep(bytes)?;
        }
        Ok(Pushed::Wrote(wrote))
    }

    /// `bytes`, which fit, kept in the buffer.
    fn keep(&mut self, bytes: &[u8]) -> Result<(), FsError> {
        self.held
            .try_extend(bytes)
            .map_err(|stopped| FsError::new(FsOp::Append, &self.file.name, FsKind::Stopped(stopped)))
    }

    /// What is held appended to the file now, unsynced, and the buffer emptied: how many
    /// bytes. On an error they stay held.
    pub fn write_out(&mut self, cx: &Cx) -> Result<usize, FsError> {
        let held = self.held.len();
        if held > 0 {
            self.file.append_unsynced(cx, self.held.as_slice(), Unsynced::No)?;
            self.held.clear();
        }
        Ok(held)
    }

    /// The writer finished: what it held appended to the file, unsynced; how many bytes. On
    /// an error they are lost with the writer, and the error says so by being returned. The
    /// one way a `Buffered` ends.
    #[blaze::consumes]
    pub fn finish(self, cx: &Cx) -> Result<usize, FsError> {
        let Buffered { file, held, capacity: _ } = self;
        if held.is_empty() {
            return Ok(0);
        }
        file.append_unsynced(cx, held.as_slice(), Unsynced::No)?;
        Ok(held.len())
    }
}

impl fmt::Debug for Buffered<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Buffered({} of {} bytes held for {})", self.held.len(), self.capacity, self.file.name)
    }
}
