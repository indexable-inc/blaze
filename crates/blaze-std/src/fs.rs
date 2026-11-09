//! Files (docs/v2/plan/design/std.md, "Capabilities"): [`Fs`] gives directories, a
//! [`Dir<P>`] gives files beneath it by relative [`Path`] only, a [`File<P>`] gives positional
//! reads and writes and its sync. `P` is [`ReadOnly`] or [`ReadWrite`]; the methods that change
//! anything are on `Dir<ReadWrite>` and `File<ReadWrite>` only, and narrowing
//! ([`Dir::subdir`], [`Dir::read_only`], [`File::read_only`]) never widens.
//!
//! **Finishing.** A `File<ReadWrite>` is linear: it holds a [`ReadWrite`], which is
//! `#[blaze::linear]`, so leaving one is a compile error and it ends with [`File::close`],
//! which syncs it and returns the sync's error. A write is not durable until a sync, and a
//! file dropped unsynced loses its writes to a crash silently: closing makes that choice
//! visible. A panic that unwinds past one closes nothing and syncs nothing: there is no handle
//! to close (effects name files), so its abandonment path is its writes left unsynced, as a
//! crash would find them. A `File<ReadOnly>` and a [`Dir`] of either rights are not linear:
//! dropping one loses nothing. The positional ops in flight, [`File::read_into`] and
//! [`File::write_from`], are [`Pending`] values.
//!
//! Each is a name under the driver's root and an `Io`: blaze-core's effects name files, not
//! handles. So a read-only narrowing is the type's alone, not the OS's, and an open is beneath
//! its directory by the name's form (no root, no `..`), not by the kernel's resolution: a
//! symbolic link inside a directory is followed by the driver. Both need effects blaze-core
//! does not have yet (an open that returns a handle with rights, resolved beneath a directory:
//! `openat2` with `RESOLVE_BENEATH` and its equals). A directory is there once a file under it
//! is (blaze-core has no effect that makes an empty one).

use std::{fmt, marker::PhantomData, sync::Arc};

use blaze_core::{Done, Effect, FileName};
use blaze_mem::Frame;

use crate::{
    Plant, Stopped,
    budget::{Budget, Over},
    cx::Cx,
    io::Io,
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
/// finished with [`File::close`] (see the module's doc): neither `Copy` nor `Clone`, and made
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
    Sync,
    Replace,
    Remove,
    Rename,
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
            FsOp::Sync => "sync",
            FsOp::Replace => "replace",
            FsOp::Remove => "remove",
            FsOp::Rename => "rename",
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
    /// The wait stopped first.
    Stopped(Stopped),
    /// The directory it came from has a quota ([`Dir::quota`]) with no room for it.
    OverQuota(Over),
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
            FsKind::Stopped(stopped) => write!(f, "bstd::fs {op} of {path}: {stopped}"),
            FsKind::OverQuota(over) => write!(f, "bstd::fs {op} of {path}: its directory's quota: {over}"),
            FsKind::Driver(said) => write!(f, "bstd::fs {op} of {path}: {said}"),
            FsKind::Answered(done) => {
                write!(f, "bstd::fs {op} of {path}: the driver answered {done} (a driver's bug)")
            }
        }
    }
}

impl std::error::Error for FsError {}

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

/// `name` synced: everything written to it before is durable once this returns.
fn sync(io: &Io, cx: &Cx, op: FsOp, name: &FileName) -> Result<(), FsError> {
    // clone: the effect's name, a refcount.
    let file = name.clone();
    match call(io, cx, op, name, |tag| Effect::Sync { file, tag })? {
        Done::Synced => Ok(()),
        done => Err(unexpected(op, name, done)),
    }
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
    /// whole-file write's, and the length a [`File::set_len`] names. A replace or a remove
    /// gives nothing back, since effects name files and bstd would have to stat before every
    /// change to know what it freed; and a positional write past a file's end is counted as
    /// its bytes, not the hole before them. So the files written through it by appends and
    /// whole-file writes never hold more than the quota; bytes held on the disk are the
    /// driver's to bound, with an effect blaze-core does not have yet.
    pub fn quota(&self, bytes: u64) -> Dir<ReadWrite> {
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
        Ok(File { io: self.io.clone(), name, quota: None, rights: ReadOnly })
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
    /// The file at `path`, read and write: closed with [`File::close`]. A missing one is
    /// [`FsKind::NotFound`]: an open never makes a file.
    pub fn open(&self, cx: &Cx, path: &Path) -> Result<File<ReadWrite>, FsError> {
        let name = self.beneath(path);
        self.found(cx, &name)?;
        // clone: the quota shared, a refcount.
        Ok(File { io: self.io.clone(), name, quota: self.quota.clone(), rights: ReadWrite(()) })
    }

    /// The file at `path` made empty, replacing what was there, and its directories made:
    /// closed with [`File::close`].
    pub fn create(&self, cx: &Cx, path: &Path) -> Result<File<ReadWrite>, FsError> {
        let name = self.beneath(path);
        replace(&self.io, cx, FsOp::Create, &name, &[])?;
        // clone: the quota shared, a refcount.
        Ok(File { io: self.io.clone(), name, quota: self.quota.clone(), rights: ReadWrite(()) })
    }

    /// The file at `path` made to hold `bytes`, durably and all at once: a reader or a crash
    /// sees the old contents or the new, never a part.
    pub fn write(&self, cx: &Cx, path: &Path, bytes: &[u8]) -> Result<(), FsError> {
        let name = self.beneath(path);
        within(&self.io, self.quota.as_deref(), FsOp::Replace, &name, bytes.len() as u64, || {
            replace(&self.io, cx, FsOp::Replace, &name, bytes)
        })
    }

    /// The planted bug of [`Plant::KvPutUnsynced`], for [`crate::kv::Kv::put`] alone: the file
    /// at `path` emptied, then `bytes` appended and never synced, so a crash keeps any prefix
    /// of them where [`Dir::write`] keeps the old contents or the new.
    pub(crate) fn write_unsynced(&self, cx: &Cx, path: &Path, bytes: &[u8]) -> Result<(), FsError> {
        let name = self.beneath(path);
        within(&self.io, self.quota.as_deref(), FsOp::Replace, &name, bytes.len() as u64, || {
            replace(&self.io, cx, FsOp::Replace, &name, &[])?;
            // clone: the effect's name, a refcount.
            let file = name.clone();
            // alloc: the effect's bytes, which blaze-core's `Effect` owns.
            let bytes = bytes.to_vec();
            match call(&self.io, cx, FsOp::Append, &name, |tag| Effect::Append { file, bytes, tag })? {
                Done::Appended => Ok(()),
                done => Err(unexpected(FsOp::Append, &name, done)),
            }
        })
    }

    /// The file at `path` gone; gone already is done.
    pub fn remove(&self, cx: &Cx, path: &Path) -> Result<(), FsError> {
        let name = self.beneath(path);
        // clone: the effect's name, a refcount.
        let file = name.clone();
        match call(&self.io, cx, FsOp::Remove, &name, |tag| Effect::Remove { file, tag })? {
            Done::Removed => Ok(()),
            done => Err(unexpected(FsOp::Remove, &name, done)),
        }
    }

    /// `from` renamed over `to`, both beneath this directory, atomically.
    pub fn rename(&self, cx: &Cx, from: &Path, to: &Path) -> Result<(), FsError> {
        let (from, to) = (self.beneath(from), self.beneath(to));
        // clone: the effect's names, refcounts.
        let (old, new) = (from.clone(), to.clone());
        match call(&self.io, cx, FsOp::Rename, &from, |tag| Effect::Rename { from: old, to: new, tag })? {
            Done::Renamed => Ok(()),
            done => Err(unexpected(FsOp::Rename, &from, done)),
        }
    }
}

/// `name` made to hold `bytes`, all at once.
fn replace(io: &Io, cx: &Cx, op: FsOp, name: &FileName, bytes: &[u8]) -> Result<(), FsError> {
    // clone: the effect's name, a refcount.
    let file = name.clone();
    // alloc: the effect's bytes, which blaze-core's `Effect` owns.
    let bytes = bytes.to_vec();
    match call(io, cx, op, name, |tag| Effect::Write { file, bytes, tag })? {
        Done::Written => Ok(()),
        done => Err(unexpected(op, name, done)),
    }
}

/// A file beneath a directory. A `File<ReadWrite>` is linear, finished with [`File::close`];
/// a `File<ReadOnly>` is not (see the module's doc).
pub struct File<#[blaze::linear] P: Rights> {
    io: Io,
    name: FileName,
    /// The quota of the directory it was opened from, which its writes count against; none
    /// on a read-only file, which writes nothing.
    quota: Option<Arc<Budget>>,
    /// The rights themselves, not a `PhantomData`: a [`ReadWrite`] held makes the file linear.
    rights: P,
}

impl Clone for File<ReadOnly> {
    fn clone(&self) -> File<ReadOnly> {
        // clone: authority held, copied: a refcount each.
        File { io: self.io.clone(), name: self.name.clone(), quota: None, rights: ReadOnly }
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
        File { io: self.io.clone(), name: self.name.clone(), quota: None, rights: ReadOnly }
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
    /// `bytes` added at the end; durable once a later [`File::sync`] has returned.
    pub fn append(&self, cx: &Cx, bytes: &[u8]) -> Result<(), FsError> {
        within(&self.io, self.quota.as_deref(), FsOp::Append, &self.name, bytes.len() as u64, || {
            // clone: the effect's name, a refcount.
            let file = self.name.clone();
            // alloc: the effect's bytes, which blaze-core's `Effect` owns.
            let bytes = bytes.to_vec();
            match call(&self.io, cx, FsOp::Append, &self.name, |tag| Effect::Append { file, bytes, tag })? {
                Done::Appended => Ok(()),
                done => Err(unexpected(FsOp::Append, &self.name, done)),
            }
        })
    }

    /// A write of `from` at `at`, past the end making it longer, in flight; durable once a
    /// later sync has returned. The bytes are copied as it starts (see [`crate::pending`]),
    /// and stay lent until it ends.
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

    /// `bytes` written at `at`, past the end making it longer; durable once a later sync has
    /// returned. Its [`File::write_from`], waited for.
    pub fn write_at(&self, cx: &Cx, at: u64, bytes: &[u8]) -> Result<(), FsError> {
        cx.stopped_by(self.io.now())
            .map_err(|stopped| FsError::new(FsOp::WriteAt, &self.name, FsKind::Stopped(stopped)))?;
        self.write_from(bytes, at).wait(cx)
    }

    /// The file cut or grown to `len`; durable once a later sync has returned.
    pub fn set_len(&self, cx: &Cx, len: u64) -> Result<(), FsError> {
        within(&self.io, self.quota.as_deref(), FsOp::SetLen, &self.name, len, || {
            // clone: the effect's name, a refcount.
            let file = self.name.clone();
            match call(&self.io, cx, FsOp::SetLen, &self.name, |tag| Effect::Truncate { file, len, tag })? {
                Done::Truncated => Ok(()),
                done => Err(unexpected(FsOp::SetLen, &self.name, done)),
            }
        })
    }

    /// Everything written to the file before this was called is durable once it returns.
    pub fn sync(&self, cx: &Cx) -> Result<(), FsError> {
        if self.io.plant() == Plant::SyncSkipped {
            return Ok(());
        }
        sync(&self.io, cx, FsOp::Sync, &self.name)
    }

    /// The file made to hold `bytes`, durably and all at once.
    pub fn replace(&self, cx: &Cx, bytes: &[u8]) -> Result<(), FsError> {
        within(&self.io, self.quota.as_deref(), FsOp::Replace, &self.name, bytes.len() as u64, || {
            replace(&self.io, cx, FsOp::Replace, &self.name, bytes)
        })
    }

    /// The file finished: synced, so everything written to it is durable, or the sync's
    /// error, op [`FsOp::Close`]. The one way a `File<ReadWrite>` ends.
    #[blaze::consumes]
    pub fn close(self, cx: &Cx) -> Result<(), FsError> {
        let File { io, name, quota: _, rights } = self;
        crate::consume(rights);
        if io.plant() == Plant::CloseSkipsSync {
            return Ok(());
        }
        sync(&io, cx, FsOp::Close, &name)
    }
}
