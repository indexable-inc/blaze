//! Files (docs/v2/plan/design/std.md, "Capabilities"): [`Fs`] gives directories, a
//! [`Dir<P>`] gives files beneath it by relative [`Path`] only, a [`File<P>`] gives positional
//! reads and writes and its sync. `P` is [`ReadOnly`] or [`ReadWrite`]; the methods that change
//! anything are gated by `P: CanWrite`, and narrowing ([`Dir::subdir`], [`Dir::read_only`],
//! [`File::read_only`]) never widens.
//!
//! Each is a name under the driver's root and an `Io`: blaze-core's effects name files, not
//! handles. So a read-only narrowing is the type's alone, not the OS's, and an open is beneath
//! its directory by the name's form (no root, no `..`), not by the kernel's resolution: a
//! symbolic link inside a directory is followed by the driver. Both need effects blaze-core
//! does not have yet (an open that returns a handle with rights, resolved beneath a directory:
//! `openat2` with `RESOLVE_BENEATH` and its equals). A directory is there once a file under it
//! is (blaze-core has no effect that makes an empty one).

use std::{fmt, marker::PhantomData};

use blaze_core::{Done, Effect, FileName};

use crate::{
    Plant, Stopped,
    cx::Cx,
    io::Io,
    mem::Bytes,
    path::Path,
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

/// May read.
pub trait CanRead: Rights {}

/// May change.
pub trait CanWrite: CanRead {}

/// Reads only.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReadOnly;

/// Reads and changes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReadWrite;

impl Rights for ReadOnly {
    const WRITE: bool = false;
}
impl Rights for ReadWrite {
    const WRITE: bool = true;
}
impl CanRead for ReadOnly {}
impl CanRead for ReadWrite {}
impl CanWrite for ReadWrite {}

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

/// What is at `name`, or `None`.
fn stat(io: &Io, cx: &Cx, op: FsOp, name: &FileName) -> Result<Option<Meta>, FsError> {
    // clone: the effect's name, a refcount.
    let file = name.clone();
    match call(io, cx, op, name, |tag| Effect::Stat { file, tag })? {
        Done::Stat(meta) => Ok(meta.map(|it| Meta { len: it.len, dir: it.dir })),
        done => Err(unexpected(op, name, done)),
    }
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
            Some(_) | None => Ok(Dir { io: self.io.clone(), base: path.name().clone(), rights: PhantomData }),
        }
    }
}

/// A directory: the files beneath it, by relative path only.
pub struct Dir<P: Rights> {
    io: Io,
    base: FileName,
    rights: PhantomData<P>,
}

impl<P: Rights> Clone for Dir<P> {
    fn clone(&self) -> Dir<P> {
        // clone: authority held, copied: a refcount each.
        Dir { io: self.io.clone(), base: self.base.clone(), rights: PhantomData }
    }
}

impl<P: Rights> fmt::Debug for Dir<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rights = if P::WRITE { "ReadWrite" } else { "ReadOnly" };
        write!(f, "Dir<{rights}>({})", self.base)
    }
}

impl<P: Rights> Dir<P> {
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
        Dir { io: self.io.clone(), base, rights: PhantomData }
    }

    /// The same directory, read only.
    pub fn read_only(&self) -> Dir<ReadOnly> {
        // clone: authority held, narrowed: a refcount each.
        Dir { io: self.io.clone(), base: self.base.clone(), rights: PhantomData }
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

    /// The file at `path`, with this directory's rights. A missing one is
    /// [`FsKind::NotFound`]: an open never makes a file.
    pub fn open(&self, cx: &Cx, path: &Path) -> Result<File<P>, FsError> {
        let name = self.beneath(path);
        self.open_name(cx, name)
    }

    fn open_name(&self, cx: &Cx, name: FileName) -> Result<File<P>, FsError> {
        match stat(&self.io, cx, FsOp::Open, &name)? {
            Some(Meta { dir: false, .. }) => Ok(File { io: self.io.clone(), name, rights: PhantomData }),
            Some(Meta { dir: true, .. }) => Err(FsError::new(FsOp::Open, &name, FsKind::IsADirectory)),
            None => {
                if self.io.plant() == Plant::OpenCreates {
                    // clone: the planted bug's effect's name, a refcount.
                    let file = name.clone();
                    // alloc: none, an empty file.
                    call(&self.io, cx, FsOp::Open, &name, |tag| Effect::Write { file, bytes: Vec::new(), tag })?;
                }
                Err(FsError::new(FsOp::Open, &name, FsKind::NotFound))
            }
        }
    }

    /// The whole file at `path` added to `into`; how many bytes it added.
    pub fn read_all(&self, cx: &Cx, path: &Path, into: &mut Bytes<'_>) -> Result<usize, FsError> {
        self.open(cx, path)?.read_all(cx, into)
    }
}

impl<P: CanWrite> Dir<P> {
    /// The file at `path` made empty, replacing what was there, and its directories made.
    pub fn create(&self, cx: &Cx, path: &Path) -> Result<File<P>, FsError> {
        let name = self.beneath(path);
        replace(&self.io, cx, FsOp::Create, &name, &[])?;
        Ok(File { io: self.io.clone(), name, rights: PhantomData })
    }

    /// The file at `path` made to hold `bytes`, durably and all at once: a reader or a crash
    /// sees the old contents or the new, never a part.
    pub fn write(&self, cx: &Cx, path: &Path, bytes: &[u8]) -> Result<(), FsError> {
        replace(&self.io, cx, FsOp::Replace, &self.beneath(path), bytes)
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

/// A file beneath a directory.
pub struct File<P: Rights> {
    io: Io,
    name: FileName,
    rights: PhantomData<P>,
}

impl<P: Rights> Clone for File<P> {
    fn clone(&self) -> File<P> {
        // clone: authority held, copied: a refcount each.
        File { io: self.io.clone(), name: self.name.clone(), rights: PhantomData }
    }
}

impl<P: Rights> fmt::Debug for File<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rights = if P::WRITE { "ReadWrite" } else { "ReadOnly" };
        write!(f, "File<{rights}>({})", self.name)
    }
}

impl<P: Rights> File<P> {
    pub fn name(&self) -> &FileName {
        &self.name
    }

    /// The same file, read only.
    pub fn read_only(&self) -> File<ReadOnly> {
        // clone: authority held, narrowed: a refcount each.
        File { io: self.io.clone(), name: self.name.clone(), rights: PhantomData }
    }

    /// Its length now.
    pub fn len(&self, cx: &Cx) -> Result<u64, FsError> {
        match stat(&self.io, cx, FsOp::Stat, &self.name)? {
            Some(meta) => Ok(meta.len),
            None => Err(FsError::new(FsOp::Stat, &self.name, FsKind::NotFound)),
        }
    }

    /// Up to `len` bytes at `at` added to `into`, fewer at the end of the file; how many.
    pub fn read_at(&self, cx: &Cx, at: u64, len: usize, into: &mut Bytes<'_>) -> Result<usize, FsError> {
        // clone: the effect's name, a refcount.
        let file = self.name.clone();
        let done = call(&self.io, cx, FsOp::Read, &self.name, |tag| Effect::Read { file, at, len, tag })?;
        self.took(done, into)
    }

    /// The whole file added to `into`; how many bytes.
    pub fn read_all(&self, cx: &Cx, into: &mut Bytes<'_>) -> Result<usize, FsError> {
        // clone: the effect's name, a refcount.
        let file = self.name.clone();
        let done = call(&self.io, cx, FsOp::Read, &self.name, |tag| Effect::ReadAll { file, tag })?;
        self.took(done, into)
    }

    /// A read's bytes, moved into the caller's allocator.
    fn took(&self, done: Done, into: &mut Bytes<'_>) -> Result<usize, FsError> {
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

impl<P: CanWrite> File<P> {
    /// `bytes` added at the end; durable once a later [`File::sync`] has returned.
    pub fn append(&self, cx: &Cx, bytes: &[u8]) -> Result<(), FsError> {
        // clone: the effect's name, a refcount.
        let file = self.name.clone();
        // alloc: the effect's bytes, which blaze-core's `Effect` owns.
        let bytes = bytes.to_vec();
        match call(&self.io, cx, FsOp::Append, &self.name, |tag| Effect::Append { file, bytes, tag })? {
            Done::Appended => Ok(()),
            done => Err(unexpected(FsOp::Append, &self.name, done)),
        }
    }

    /// `bytes` written at `at`, past the end making it longer; durable once a later sync has
    /// returned.
    pub fn write_at(&self, cx: &Cx, at: u64, bytes: &[u8]) -> Result<(), FsError> {
        // clone: the effect's name, a refcount.
        let file = self.name.clone();
        // alloc: the effect's bytes, which blaze-core's `Effect` owns.
        let bytes = bytes.to_vec();
        match call(&self.io, cx, FsOp::WriteAt, &self.name, |tag| Effect::WriteAt { file, at, bytes, tag })? {
            Done::Written => Ok(()),
            done => Err(unexpected(FsOp::WriteAt, &self.name, done)),
        }
    }

    /// The file cut or grown to `len`; durable once a later sync has returned.
    pub fn set_len(&self, cx: &Cx, len: u64) -> Result<(), FsError> {
        // clone: the effect's name, a refcount.
        let file = self.name.clone();
        match call(&self.io, cx, FsOp::SetLen, &self.name, |tag| Effect::Truncate { file, len, tag })? {
            Done::Truncated => Ok(()),
            done => Err(unexpected(FsOp::SetLen, &self.name, done)),
        }
    }

    /// Everything written to the file before this was called is durable once it returns.
    pub fn sync(&self, cx: &Cx) -> Result<(), FsError> {
        if self.io.plant() == Plant::SyncSkipped {
            return Ok(());
        }
        // clone: the effect's name, a refcount.
        let file = self.name.clone();
        match call(&self.io, cx, FsOp::Sync, &self.name, |tag| Effect::Sync { file, tag })? {
            Done::Synced => Ok(()),
            done => Err(unexpected(FsOp::Sync, &self.name, done)),
        }
    }

    /// The file made to hold `bytes`, durably and all at once.
    pub fn replace(&self, cx: &Cx, bytes: &[u8]) -> Result<(), FsError> {
        replace(&self.io, cx, FsOp::Replace, &self.name, bytes)
    }
}
