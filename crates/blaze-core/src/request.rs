//! Zero-alloc typed linear requests (`blaze_machine::Request`), strong newtypes (`ByteOffset`,
//! `ByteLen`, `MaxBytes`), `#[blaze::args]` `*Opts` builders, owned DMA buffers (`DmaBuf`), and
//! [`SimHandler`] driving machines directly over [`crate::sim::Sim`]
//! (`docs/v2/plan/design/sans-io-machines.md`, sections 3–4).
//!
//! Key guarantees:
//! - Every DMA request (`ReadInto`, `WriteFrom`, `ReadAll`) takes an owned caller [`DmaBuf`] and
//!   returns it in its answer (`FilledBuf` or `BufError`); the driver never allocates a `Vec<u8>`
//!   on the global heap for a caller.
//! - Lent-slice [`Read`] is capped to [`LENT_READ_MAX`] (backed by a fixed driver scratch buffer).
//! - Absent files return [`FsError::NotFound`], never an empty byte slice.
//! - Typestate durability: writable files open as `File<'io, Unflushed>`, `Flush` consumes
//!   `Unflushed` and returns `File<'io, Flushed>` in its answer, and `RenameFlushed` requires a
//!   `FlushedProof` minted only by `Flush`—making durability ordering checkable in types.

use core::fmt;

use blaze_machine::{Handler, Perform, Request};
use blaze_own::{Alloc, Vec};

use crate::handle::{
    Dir, DirKind, ExitKind, File, FileId, FileKind, Flushed, HandleError, HandleTable,
    HandleTableOpts, LeakedHandles, ReadOnly, ReadWrite, TaskId, Unflushed,
};
use crate::{DirName, Done, Driver, Effect, FileName, Tag, sim::Sim, wait::Deadline};

// ---------------------------------------------------------------------------
// Strong newtypes (rule 73) and `#[blaze::args]` options structs (rule 83)
// ---------------------------------------------------------------------------

/// Byte offset inside a file.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct ByteOffset(pub u64);

/// Exact byte length of one IO transfer.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct ByteLen(pub u32);

/// Explicit upper byte cap for variable-size reads (`ReadAll`, rule 18 `bounded-progress`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MaxBytes(pub u64);

/// Hard upper bound for a lent-slice [`Read`] served from the driver's per-worker scratch buffer.
pub const LENT_READ_MAX: ByteLen = ByteLen(64 * 1024);

/// Caller-owned DMA buffer backed by an explicit caller allocator (`blaze_own::Vec`), moved into
/// a request and returned in its answer (zero global heap allocation).
#[blaze::linear]
#[derive(Debug)]
pub struct DmaBuf<'a> {
    bytes: Vec<u8, Alloc<'a>>,
}

impl<'a> DmaBuf<'a> {
    /// Allocates an empty DMA buffer with capacity `cap` in `alloc`.
    #[inline]
    pub fn with_capacity_in(cap: ByteLen, alloc: Alloc<'a>) -> Self {
        Self { bytes: Vec::with_capacity_in(cap.0 as usize, alloc) }
    }

    /// Creates a DMA buffer initialized with `data` copied into `alloc`.
    #[inline]
    pub fn from_slice_in(data: &[u8], alloc: Alloc<'a>) -> Self {
        let mut bytes = Vec::with_capacity_in(data.len(), alloc);
        bytes.extend_from_slice(data);
        Self { bytes }
    }

    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        self.bytes.as_mut_slice()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    #[inline]
    pub fn clear(&mut self) {
        self.bytes.clear();
    }

    #[inline]
    pub fn resize_zero(&mut self, len: usize) {
        self.bytes.clear();
        self.bytes.resize(len, 0);
    }

    /// Consumes this linear buffer explicitly (freeing through its named allocator).
    #[blaze::consumes]
    #[inline]
    pub fn release(self) {
        drop(self.bytes);
    }
}

/// Filled DMA buffer returned by `ReadInto` / `ReadAll`.
#[derive(Debug)]
pub struct FilledBuf<'a> {
    pub buf: DmaBuf<'a>,
    pub filled: ByteLen,
}

/// Error carrying back the caller's owned [`DmaBuf`] so no allocation is ever lost on failure.
#[derive(Debug)]
pub struct BufError<'a, E> {
    pub buf: DmaBuf<'a>,
    pub error: E,
}

/// Typed filesystem error: absent files are ALWAYS [`FsError::NotFound`], never empty bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FsError {
    NotFound { path: FileName },
    LimitExceeded { requested: u64, max: u64 },
    NoSpace { path: FileName },
    StaleHandle(HandleError),
    Io { path: Option<FileName>, message: std::string::String },
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::NotFound { path } => write!(f, "file not found: {path}"),
            FsError::LimitExceeded { requested, max } => {
                write!(f, "read length {requested} exceeds cap {max}")
            }
            FsError::NoSpace { path } => write!(f, "no space left writing {path}"),
            FsError::StaleHandle(e) => write!(f, "stale handle: {e}"),
            FsError::Io { path: Some(p), message } => write!(f, "io error on {p}: {message}"),
            FsError::Io { path: None, message } => write!(f, "io error: {message}"),
        }
    }
}

/// Zero-size proof token minted only when [`Flush`] completes durably for `path` (rule 74
/// `verify-once`). Required by [`RenameFlushed`] so forgetting `fsync` before `rename` is a
/// compile error.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FlushedProof {
    path: FileName,
}

impl FlushedProof {
    #[inline]
    pub fn path(&self) -> &FileName {
        &self.path
    }
}

// ---------------------------------------------------------------------------
// Options structs (`#[blaze::args]`, rule 83)
// ---------------------------------------------------------------------------

/// Options for opening a file under a directory handle.
#[blaze::args]
pub struct OpenOpts {
    pub path: FileName,
    pub create: bool,
    pub truncate: bool,
}

/// Options for a capped lent-slice read.
#[blaze::args]
pub struct ReadOpts {
    pub at: ByteOffset,
    pub len: ByteLen,
}

/// Options for an owned-buffer positional read.
#[blaze::args]
pub struct ReadIntoOpts<'a> {
    pub at: ByteOffset,
    pub buf: DmaBuf<'a>,
}

/// Options for an owned-buffer positional write.
#[blaze::args]
pub struct WriteFromOpts<'a> {
    pub at: ByteOffset,
    pub buf: DmaBuf<'a>,
}

/// Options for reading a whole file into a caller buffer with an explicit byte cap.
#[blaze::args]
pub struct ReadAllOpts<'a> {
    pub path: FileName,
    pub max: MaxBytes,
    pub buf: DmaBuf<'a>,
}

/// Options for renaming a file whose bytes have already been durably flushed.
#[blaze::args]
pub struct RenameFlushedOpts {
    pub flushed: FlushedProof,
    pub to: FileName,
}

/// Options for an explicitly unsynced rename (named for its risk, rule 73 & 92).
#[blaze::args]
pub struct RenameUnsyncedOpts {
    pub from: FileName,
    pub to: FileName,
}

// ---------------------------------------------------------------------------
// Typed linear requests implementing `blaze_machine::Request`
// ---------------------------------------------------------------------------

#[blaze::linear]
#[derive(Debug)]
pub struct OpenWrite<'io> {
    pub(crate) dir: Dir<'io, ReadWrite>,
    pub(crate) path: FileName,
    pub(crate) create: bool,
    pub(crate) truncate: bool,
}

impl<'io> Request for OpenWrite<'io> {
    type Answer<'s> = Result<File<'io, Unflushed>, FsError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct OpenRead<'io> {
    pub(crate) dir: Dir<'io, ReadOnly>,
    pub(crate) path: FileName,
}

impl<'io> Request for OpenRead<'io> {
    type Answer<'s> = Result<File<'io, ReadOnly>, FsError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct Close<'io> {
    pub(crate) file: FileId<'io>,
}

impl<'io> Request for Close<'io> {
    type Answer<'s> = Result<(), FsError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct Read<'io> {
    pub(crate) file: FileId<'io>,
    pub(crate) at: ByteOffset,
    pub(crate) len: ByteLen,
}

impl<'io> Request for Read<'io> {
    type Answer<'s> = Result<&'s [u8], FsError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct ReadInto<'io, 'a> {
    pub(crate) file: FileId<'io>,
    pub(crate) at: ByteOffset,
    pub(crate) buf: DmaBuf<'a>,
}

impl<'io, 'a> Request for ReadInto<'io, 'a> {
    type Answer<'s> = Result<FilledBuf<'a>, BufError<'a, FsError>>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct WriteFrom<'io, 'a> {
    pub(crate) file: FileId<'io>,
    pub(crate) at: ByteOffset,
    pub(crate) buf: DmaBuf<'a>,
}

impl<'io, 'a> Request for WriteFrom<'io, 'a> {
    type Answer<'s> = Result<DmaBuf<'a>, BufError<'a, FsError>>;
}

/// Consumes an [`Unflushed`] file handle and returns `(File<'io, Flushed>, FlushedProof)` once
/// durable on disk, or hands back the [`Unflushed`] handle beside the error so the caller can
/// close it linearly.
#[blaze::linear]
#[derive(Debug)]
pub struct Flush<'io> {
    pub(crate) file: File<'io, Unflushed>,
}

impl<'io> Request for Flush<'io> {
    type Answer<'s> = Result<(File<'io, Flushed>, FlushedProof), (File<'io, Unflushed>, FsError)>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct ReadAll<'io, 'a> {
    pub(crate) dir: Dir<'io, ReadOnly>,
    pub(crate) path: FileName,
    pub(crate) max: MaxBytes,
    pub(crate) buf: DmaBuf<'a>,
}

impl<'io, 'a> Request for ReadAll<'io, 'a> {
    type Answer<'s> = Result<FilledBuf<'a>, BufError<'a, FsError>>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct RenameFlushed<'io> {
    pub(crate) dir: Dir<'io, ReadWrite>,
    pub(crate) flushed: FlushedProof,
    pub(crate) to: FileName,
}

impl<'io> Request for RenameFlushed<'io> {
    type Answer<'s> = Result<(), FsError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct RenameUnsynced<'io> {
    pub(crate) dir: Dir<'io, ReadWrite>,
    pub(crate) from: FileName,
    pub(crate) to: FileName,
}

impl<'io> Request for RenameUnsynced<'io> {
    type Answer<'s> = Result<(), FsError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct SyncDir<'io> {
    pub(crate) dir: Dir<'io, ReadWrite>,
}

impl<'io> Request for SyncDir<'io> {
    type Answer<'s> = Result<(), FsError>;
}

// ---------------------------------------------------------------------------
// Handle methods that build inert requests (take no `cx`, never wait)
// ---------------------------------------------------------------------------

impl<'io> Dir<'io, ReadWrite> {
    #[inline]
    pub fn open_write(&self, opts: OpenOpts) -> OpenWrite<'io> {
        OpenWrite { dir: *self, path: opts.path, create: opts.create, truncate: opts.truncate }
    }

    #[inline]
    pub fn rename_flushed(&self, opts: RenameFlushedOpts) -> RenameFlushed<'io> {
        RenameFlushed { dir: *self, flushed: opts.flushed, to: opts.to }
    }

    #[inline]
    pub fn rename_unsynced(&self, opts: RenameUnsyncedOpts) -> RenameUnsynced<'io> {
        RenameUnsynced { dir: *self, from: opts.from, to: opts.to }
    }

    #[inline]
    pub fn sync_dir(&self) -> SyncDir<'io> {
        SyncDir { dir: *self }
    }
}

impl<'io> Dir<'io, ReadOnly> {
    #[inline]
    pub fn open_read(&self, path: FileName) -> OpenRead<'io> {
        OpenRead { dir: *self, path }
    }

    #[inline]
    pub fn read_all<'a>(&self, opts: ReadAllOpts<'a>) -> ReadAll<'io, 'a> {
        ReadAll { dir: *self, path: opts.path, max: opts.max, buf: opts.buf }
    }
}

impl<'io, Mode> File<'io, Mode> {
    #[inline]
    pub fn read(&self, opts: ReadOpts) -> Read<'io> {
        Read { file: self.id(), at: opts.at, len: opts.len }
    }

    #[inline]
    pub fn read_into<'a>(&self, opts: ReadIntoOpts<'a>) -> ReadInto<'io, 'a> {
        ReadInto { file: self.id(), at: opts.at, buf: opts.buf }
    }

    /// Consumes the linear `File` handle into a [`Close`] request.
    #[blaze::consumes]
    #[inline]
    pub fn close(self) -> Close<'io> {
        Close { file: self.into_id() }
    }
}

impl<'io> File<'io, Unflushed> {
    #[inline]
    pub fn write_from<'a>(&self, opts: WriteFromOpts<'a>) -> WriteFrom<'io, 'a> {
        WriteFrom { file: self.id(), at: opts.at, buf: opts.buf }
    }

    /// Consumes `File<'io, Unflushed>` into a [`Flush`] request that mints `File<'io, Flushed>`
    /// and a [`FlushedProof`] on success.
    #[blaze::consumes]
    #[inline]
    pub fn flush(self) -> Flush<'io> {
        Flush { file: self }
    }
}

// ---------------------------------------------------------------------------
// Unified request/answer union for Rust-edition machines & `SimHandler`
// ---------------------------------------------------------------------------

#[blaze::linear]
#[derive(Debug)]
pub enum FsRequest<'io, 'a> {
    OpenWrite(OpenWrite<'io>),
    OpenRead(OpenRead<'io>),
    Close(Close<'io>),
    Read(Read<'io>),
    ReadInto(ReadInto<'io, 'a>),
    WriteFrom(WriteFrom<'io, 'a>),
    Flush(Flush<'io>),
    ReadAll(ReadAll<'io, 'a>),
    RenameFlushed(RenameFlushed<'io>),
    RenameUnsynced(RenameUnsynced<'io>),
    SyncDir(SyncDir<'io>),
}

#[derive(Debug)]
pub enum FsAnswer<'io, 'a, 's> {
    OpenedWrite(Result<File<'io, Unflushed>, FsError>),
    OpenedRead(Result<File<'io, ReadOnly>, FsError>),
    Closed(Result<(), FsError>),
    Read(Result<&'s [u8], FsError>),
    ReadInto(Result<FilledBuf<'a>, BufError<'a, FsError>>),
    Written(Result<DmaBuf<'a>, BufError<'a, FsError>>),
    Flushed(Result<(File<'io, Flushed>, FlushedProof), (File<'io, Unflushed>, FsError)>),
    ReadAll(Result<FilledBuf<'a>, BufError<'a, FsError>>),
    Renamed(Result<(), FsError>),
    SyncedDir(Result<(), FsError>),
}

impl<'io, 'a> Request for FsRequest<'io, 'a> {
    type Answer<'s> = FsAnswer<'io, 'a, 's>;
}

/// Driver-level error from [`SimHandler`] (e.g. a task exited normally with leaked handles).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SimHandlerError {
    LeakedHandles(LeakedHandles),
    TableFull(HandleError),
}

impl fmt::Display for SimHandlerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SimHandlerError::LeakedHandles(l) => write!(f, "{l}"),
            SimHandlerError::TableFull(e) => write!(f, "{e}"),
        }
    }
}

struct OpenFileEntry {
    path: FileName,
    writable: bool,
}

/// Deterministic `blaze_machine::Handler` driving typed [`FsRequest`]s directly over
/// [`crate::sim::Sim`] with generational branded handles, capped scratch buffers, explicit
/// `NotFound`, and loud normal-exit leak detection.
pub struct SimHandler<'io, 'sim> {
    sim: &'sim mut Sim,
    task: TaskId,
    files: HandleTable<'io, FileKind, OpenFileEntry>,
    dirs: HandleTable<'io, DirKind, DirName>,
    root_dir: Dir<'io, ReadWrite>,
    scratch: Vec<u8, Alloc<'io>>,
    next_tag: u64,
}

impl<'io, 'sim> SimHandler<'io, 'sim> {
    /// Creates a `SimHandler` bound to `sim`, branded with `brand`, and backed by `alloc`.
    pub fn new(
        sim: &'sim mut Sim,
        task: TaskId,
        brand: blaze_own::Brand<'io>,
        alloc: Alloc<'io>,
    ) -> Self {
        let files = HandleTable::new(HandleTableOpts { brand, max_slots: 256, alloc });
        let mut dirs = HandleTable::new(HandleTableOpts { brand, max_slots: 64, alloc });
        let root_id = dirs.insert(task, DirName::Root).expect("root dir slot");
        let root_dir = Dir::from_id(root_id);
        let scratch = Vec::with_capacity_in(LENT_READ_MAX.0 as usize, alloc);
        Self { sim, task, files, dirs, root_dir, scratch, next_tag: 1 }
    }

    #[inline]
    pub const fn root(&self) -> Dir<'io, ReadWrite> {
        self.root_dir
    }

    #[inline]
    pub fn sim_mut(&mut self) -> &mut Sim {
        self.sim
    }

    /// Sweeps open file handles after a task finishes: returns `Err(LeakedHandles)` on normal exit
    /// if any file handle was left unclosed.
    pub fn finish_task(&mut self, exit: ExitKind) -> Result<u32, SimHandlerError> {
        self.files.reclaim_task(self.task, exit, |_| {}).map_err(SimHandlerError::LeakedHandles)
    }

    fn run_effect(&mut self, effect: Effect) -> Done {
        let tag = Tag(self.next_tag);
        self.next_tag += 1;
        let _ = tag;
        let mut submit = std::vec![effect];
        self.sim.submit(&mut submit);
        let mut out = std::vec::Vec::new();
        while out.is_empty() {
            self.sim.poll(&mut out, Deadline::NEVER);
        }
        out.pop().expect("one completion").done
    }

    fn alloc_tag(&mut self) -> Tag {
        let t = Tag(self.next_tag);
        self.next_tag += 1;
        t
    }

    fn map_io_err(path: FileName, err: crate::IoError) -> FsError {
        let msg = err.to_string();
        if msg.contains("No space") || msg.contains("ENOSPC") || msg.contains("full") {
            FsError::NoSpace { path }
        } else if msg.contains("NotFound") || msg.contains("not found") || msg.contains("No such file") {
            FsError::NotFound { path }
        } else {
            FsError::Io { path: Some(path), message: msg }
        }
    }
}

impl<'io, 'a, 'sim> Handler<FsRequest<'io, 'a>> for SimHandler<'io, 'sim> {
    type Error = SimHandlerError;

    fn perform<Out>(
        &mut self,
        request: FsRequest<'io, 'a>,
        deliver: impl FnOnce(Perform<FsAnswer<'io, 'a, '_>>) -> Out,
    ) -> Result<Out, Self::Error> {
        match request {
            FsRequest::OpenWrite(req) => {
                if let Err(e) = self.dirs.get(req.dir.id()) {
                    return Ok(deliver(Perform::Answered(FsAnswer::OpenedWrite(Err(
                        FsError::StaleHandle(e),
                    )))));
                }
                if req.truncate {
                    let tag = self.alloc_tag();
                    match self.run_effect(Effect::Truncate { file: req.path.clone(), len: 0, tag }) {
                        Done::Truncated => {}
                        Done::Failed(e) => {
                            let err = Self::map_io_err(req.path, e);
                            return Ok(deliver(Perform::Answered(FsAnswer::OpenedWrite(Err(err)))));
                        }
                        other => panic!("unexpected Truncate completion: {other:?}"),
                    }
                }
                let id = self
                    .files
                    .insert(self.task, OpenFileEntry { path: req.path, writable: true })
                    .map_err(SimHandlerError::TableFull)?;
                Ok(deliver(Perform::Answered(FsAnswer::OpenedWrite(Ok(File::from_id(id))))))
            }
            FsRequest::OpenRead(req) => {
                if let Err(e) = self.dirs.get(req.dir.id()) {
                    return Ok(deliver(Perform::Answered(FsAnswer::OpenedRead(Err(
                        FsError::StaleHandle(e),
                    )))));
                }
                let tag = self.alloc_tag();
                match self.run_effect(Effect::Stat { file: req.path.clone(), tag }) {
                    Done::Stat(Some(_)) => {}
                    Done::Stat(None) => {
                        return Ok(deliver(Perform::Answered(FsAnswer::OpenedRead(Err(
                            FsError::NotFound { path: req.path },
                        )))));
                    }
                    Done::Failed(e) => {
                        let err = Self::map_io_err(req.path, e);
                        return Ok(deliver(Perform::Answered(FsAnswer::OpenedRead(Err(err)))));
                    }
                    other => panic!("unexpected Stat completion: {other:?}"),
                }
                let id = self
                    .files
                    .insert(self.task, OpenFileEntry { path: req.path, writable: false })
                    .map_err(SimHandlerError::TableFull)?;
                Ok(deliver(Perform::Answered(FsAnswer::OpenedRead(Ok(File::from_id(id))))))
            }
            FsRequest::Close(req) => {
                let res = self.files.remove(req.file, self.task).map(|_| ()).map_err(FsError::StaleHandle);
                Ok(deliver(Perform::Answered(FsAnswer::Closed(res))))
            }
            FsRequest::Read(req) => {
                if req.len.0 > LENT_READ_MAX.0 {
                    return Ok(deliver(Perform::Answered(FsAnswer::Read(Err(
                        FsError::LimitExceeded {
                            requested: u64::from(req.len.0),
                            max: u64::from(LENT_READ_MAX.0),
                        },
                    )))));
                }
                let path = match self.files.get(req.file) {
                    Ok(entry) => entry.path.clone(),
                    Err(e) => {
                        return Ok(deliver(Perform::Answered(FsAnswer::Read(Err(
                            FsError::StaleHandle(e),
                        )))));
                    }
                };
                let tag = self.alloc_tag();
                match self.run_effect(Effect::Read {
                    file: path.clone(),
                    at: req.at.0,
                    len: req.len.0 as usize,
                    tag,
                }) {
                    Done::Read(bytes) => {
                        self.scratch.clear();
                        self.scratch.extend_from_slice(&bytes);
                        let slice = self.scratch.as_slice();
                        Ok(deliver(Perform::Answered(FsAnswer::Read(Ok(slice)))))
                    }
                    Done::Failed(e) => {
                        let err = Self::map_io_err(path, e);
                        Ok(deliver(Perform::Answered(FsAnswer::Read(Err(err)))))
                    }
                    other => panic!("unexpected Read completion: {other:?}"),
                }
            }
            FsRequest::ReadInto(mut req) => {
                let path = match self.files.get(req.file) {
                    Ok(entry) => entry.path.clone(),
                    Err(e) => {
                        return Ok(deliver(Perform::Answered(FsAnswer::ReadInto(Err(BufError {
                            buf: req.buf,
                            error: FsError::StaleHandle(e),
                        })))));
                    }
                };
                let want = req.buf.len();
                let tag = self.alloc_tag();
                match self.run_effect(Effect::Read { file: path.clone(), at: req.at.0, len: want, tag }) {
                    Done::Read(bytes) => {
                        let n = bytes.len().min(want);
                        req.buf.as_mut_slice()[..n].copy_from_slice(&bytes[..n]);
                        Ok(deliver(Perform::Answered(FsAnswer::ReadInto(Ok(FilledBuf {
                            buf: req.buf,
                            filled: ByteLen(n as u32),
                        })))))
                    }
                    Done::Failed(e) => {
                        let err = Self::map_io_err(path, e);
                        Ok(deliver(Perform::Answered(FsAnswer::ReadInto(Err(BufError {
                            buf: req.buf,
                            error: err,
                        })))))
                    }
                    other => panic!("unexpected ReadInto completion: {other:?}"),
                }
            }
            FsRequest::WriteFrom(req) => {
                let path = match self.files.get(req.file) {
                    Ok(entry) if entry.writable => entry.path.clone(),
                    Ok(entry) => {
                        return Ok(deliver(Perform::Answered(FsAnswer::Written(Err(BufError {
                            buf: req.buf,
                            error: FsError::Io {
                                path: Some(entry.path.clone()),
                                message: "file handle is read-only".into(),
                            },
                        })))));
                    }
                    Err(e) => {
                        return Ok(deliver(Perform::Answered(FsAnswer::Written(Err(BufError {
                            buf: req.buf,
                            error: FsError::StaleHandle(e),
                        })))));
                    }
                };
                let tag = self.alloc_tag();
                // Use Append for offset-sequential writes so `Sim::crash` exercises unsynced tearing.
                let mut payload = std::vec::Vec::with_capacity(req.buf.len());
                payload.extend_from_slice(req.buf.as_slice());
                match self.run_effect(Effect::Append { file: path.clone(), bytes: payload, tag }) {
                    Done::Appended => Ok(deliver(Perform::Answered(FsAnswer::Written(Ok(req.buf))))),
                    Done::Failed(e) => {
                        let err = Self::map_io_err(path, e);
                        Ok(deliver(Perform::Answered(FsAnswer::Written(Err(BufError {
                            buf: req.buf,
                            error: err,
                        })))))
                    }
                    other => panic!("unexpected Append completion: {other:?}"),
                }
            }
            FsRequest::Flush(req) => {
                let path = match self.files.get(req.file.id()) {
                    Ok(entry) => entry.path.clone(),
                    Err(e) => {
                        return Ok(deliver(Perform::Answered(FsAnswer::Flushed(Err((
                            req.file,
                            FsError::StaleHandle(e),
                        ))))));
                    }
                };
                let tag = self.alloc_tag();
                match self.run_effect(Effect::Sync { file: path.clone(), tag }) {
                    Done::Synced => {
                        let id = req.file.into_id();
                        let flushed = File::<Flushed>::from_id(id);
                        let proof = FlushedProof { path };
                        Ok(deliver(Perform::Answered(FsAnswer::Flushed(Ok((flushed, proof))))))
                    }
                    Done::Failed(e) => {
                        let err = Self::map_io_err(path, e);
                        Ok(deliver(Perform::Answered(FsAnswer::Flushed(Err((req.file, err))))))
                    }
                    other => panic!("unexpected Sync completion: {other:?}"),
                }
            }
            FsRequest::ReadAll(mut req) => {
                if let Err(e) = self.dirs.get(req.dir.id()) {
                    return Ok(deliver(Perform::Answered(FsAnswer::ReadAll(Err(BufError {
                        buf: req.buf,
                        error: FsError::StaleHandle(e),
                    })))));
                }
                // Explicitly check existence so absent is NEVER misread as empty!
                let stat_tag = self.alloc_tag();
                match self.run_effect(Effect::Stat { file: req.path.clone(), tag: stat_tag }) {
                    Done::Stat(None) => {
                        return Ok(deliver(Perform::Answered(FsAnswer::ReadAll(Err(BufError {
                            buf: req.buf,
                            error: FsError::NotFound { path: req.path },
                        })))));
                    }
                    Done::Stat(Some(meta)) if meta.len > req.max.0 => {
                        return Ok(deliver(Perform::Answered(FsAnswer::ReadAll(Err(BufError {
                            buf: req.buf,
                            error: FsError::LimitExceeded { requested: meta.len, max: req.max.0 },
                        })))));
                    }
                    Done::Stat(Some(_)) => {}
                    Done::Failed(e) => {
                        let err = Self::map_io_err(req.path, e);
                        return Ok(deliver(Perform::Answered(FsAnswer::ReadAll(Err(BufError {
                            buf: req.buf,
                            error: err,
                        })))));
                    }
                    other => panic!("unexpected Stat completion: {other:?}"),
                }
                let tag = self.alloc_tag();
                match self.run_effect(Effect::ReadAll { file: req.path.clone(), tag }) {
                    Done::Read(bytes) => {
                        if (bytes.len() as u64) > req.max.0 {
                            return Ok(deliver(Perform::Answered(FsAnswer::ReadAll(Err(BufError {
                                buf: req.buf,
                                error: FsError::LimitExceeded {
                                    requested: bytes.len() as u64,
                                    max: req.max.0,
                                },
                            })))));
                        }
                        req.buf.clear();
                        req.buf.bytes.extend_from_slice(&bytes);
                        let filled = ByteLen(bytes.len() as u32);
                        Ok(deliver(Perform::Answered(FsAnswer::ReadAll(Ok(FilledBuf {
                            buf: req.buf,
                            filled,
                        })))))
                    }
                    Done::Failed(e) => {
                        let err = Self::map_io_err(req.path, e);
                        Ok(deliver(Perform::Answered(FsAnswer::ReadAll(Err(BufError {
                            buf: req.buf,
                            error: err,
                        })))))
                    }
                    other => panic!("unexpected ReadAll completion: {other:?}"),
                }
            }
            FsRequest::RenameFlushed(req) => {
                if let Err(e) = self.dirs.get(req.dir.id()) {
                    return Ok(deliver(Perform::Answered(FsAnswer::Renamed(Err(
                        FsError::StaleHandle(e),
                    )))));
                }
                let tag = self.alloc_tag();
                match self.run_effect(Effect::Rename {
                    from: req.flushed.path.clone(),
                    to: req.to.clone(),
                    tag,
                }) {
                    Done::Renamed => Ok(deliver(Perform::Answered(FsAnswer::Renamed(Ok(()))))),
                    Done::Failed(e) => {
                        let err = Self::map_io_err(req.to, e);
                        Ok(deliver(Perform::Answered(FsAnswer::Renamed(Err(err)))))
                    }
                    other => panic!("unexpected Rename completion: {other:?}"),
                }
            }
            FsRequest::RenameUnsynced(req) => {
                if let Err(e) = self.dirs.get(req.dir.id()) {
                    return Ok(deliver(Perform::Answered(FsAnswer::Renamed(Err(
                        FsError::StaleHandle(e),
                    )))));
                }
                let tag = self.alloc_tag();
                match self.run_effect(Effect::RenameUnsynced {
                    from: req.from,
                    to: req.to.clone(),
                    tag,
                }) {
                    Done::Renamed => Ok(deliver(Perform::Answered(FsAnswer::Renamed(Ok(()))))),
                    Done::Failed(e) => {
                        let err = Self::map_io_err(req.to, e);
                        Ok(deliver(Perform::Answered(FsAnswer::Renamed(Err(err)))))
                    }
                    other => panic!("unexpected RenameUnsynced completion: {other:?}"),
                }
            }
            FsRequest::SyncDir(req) => {
                let dir_name = match self.dirs.get(req.dir.id()) {
                    Ok(d) => d.clone(),
                    Err(e) => {
                        return Ok(deliver(Perform::Answered(FsAnswer::SyncedDir(Err(
                            FsError::StaleHandle(e),
                        )))));
                    }
                };
                let tag = self.alloc_tag();
                match self.run_effect(Effect::SyncDir { dir: dir_name, tag }) {
                    Done::Synced => Ok(deliver(Perform::Answered(FsAnswer::SyncedDir(Ok(()))))),
                    Done::Failed(e) => Ok(deliver(Perform::Answered(FsAnswer::SyncedDir(Err(
                        FsError::Io { path: None, message: e.to_string() },
                    ))))),
                    other => panic!("unexpected SyncDir completion: {other:?}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use blaze_machine::{DriveError, DriveOpts, Machine, Turn, drive};
    use blaze_own::testing::heap;
    use blaze_own::with_brand;
    use blaze_sim::{Broke, Config};

    use super::{
        ByteLen, ByteOffset, DmaBuf, FlushedProof, FsAnswer, FsError, FsRequest, LENT_READ_MAX,
        MaxBytes, OpenOpts, ReadAllOpts, ReadOpts, RenameFlushedOpts, RenameUnsyncedOpts,
        SimHandler, SimHandlerError, WriteFromOpts,
    };
    use crate::handle::{Dir, ExitKind, File, ReadWrite, TaskId, Unflushed};
    use crate::sim::{Faults, Sim};
    use crate::FileName;

    #[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
    enum DurabilityPlant {
        #[default]
        None,
        /// Planted bug: skip `Flush` and `SyncDir`, using `RenameUnsynced` instead.
        SkipFlushAndSync,
    }

    /// Real durable atomic-write machine driven by `SimHandler`:
    /// `OpenWrite(tmp) -> WriteFrom(tmp, buf) -> Flush(tmp) -> Close(tmp) -> RenameFlushed(tmp -> target) -> SyncDir(root)`
    /// Closes `tmp` linearly on EVERY error path!
    enum AtomicWrite<'io, 'a> {
        Start {
            dir: Dir<'io, ReadWrite>,
            tmp: FileName,
            target: FileName,
            buf: DmaBuf<'a>,
            plant: DurabilityPlant,
        },
        Opening {
            dir: Dir<'io, ReadWrite>,
            tmp: FileName,
            target: FileName,
            buf: DmaBuf<'a>,
            plant: DurabilityPlant,
        },
        Writing {
            dir: Dir<'io, ReadWrite>,
            tmp: FileName,
            target: FileName,
            file: File<'io, Unflushed>,
            plant: DurabilityPlant,
        },
        Flushing {
            dir: Dir<'io, ReadWrite>,
            target: FileName,
        },
        ClosingBeforeRename {
            dir: Dir<'io, ReadWrite>,
            target: FileName,
            proof: Option<FlushedProof>,
            unsynced_from: Option<FileName>,
        },
        ClosingOnError {
            err: FsError,
        },
        Renaming {
            dir: Dir<'io, ReadWrite>,
            sync_dir: bool,
        },
        SyncingDir,
    }

    impl<'io, 'a> Machine for AtomicWrite<'io, 'a> {
        type Req = FsRequest<'io, 'a>;
        type Out = ();
        type Err = FsError;

        fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
            match self {
                AtomicWrite::Start { dir, tmp, target, buf, plant } => {
                    let req = dir.open_write(OpenOpts { path: tmp.clone(), create: true, truncate: true });
                    Turn::Ask {
                        request: FsRequest::OpenWrite(req),
                        next: AtomicWrite::Opening { dir, tmp, target, buf, plant },
                    }
                }
                _ => unreachable!(),
            }
        }

        fn resume(
            self,
            answer: FsAnswer<'io, 'a, '_>,
            _cx: &mut (),
        ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
            match (self, answer) {
                (
                    AtomicWrite::Opening { dir, tmp, target, buf, plant },
                    FsAnswer::OpenedWrite(Ok(file)),
                ) => {
                    let req = file.write_from(WriteFromOpts { at: ByteOffset(0), buf });
                    Turn::Ask {
                        request: FsRequest::WriteFrom(req),
                        next: AtomicWrite::Writing { dir, tmp, target, file, plant },
                    }
                }
                (AtomicWrite::Opening { buf, .. }, FsAnswer::OpenedWrite(Err(e))) => {
                    buf.release();
                    Turn::Fail(e)
                }
                (
                    AtomicWrite::Writing { dir, tmp, target, file, plant },
                    FsAnswer::Written(Ok(buf)),
                ) => {
                    buf.release();
                    if plant == DurabilityPlant::SkipFlushAndSync {
                        Turn::Ask {
                            request: FsRequest::Close(file.close()),
                            next: AtomicWrite::ClosingBeforeRename {
                                dir,
                                target,
                                proof: None,
                                unsynced_from: Some(tmp),
                            },
                        }
                    } else {
                        Turn::Ask {
                            request: FsRequest::Flush(file.flush()),
                            next: AtomicWrite::Flushing { dir, target },
                        }
                    }
                }
                (AtomicWrite::Writing { file, .. }, FsAnswer::Written(Err(be))) => {
                    be.buf.release();
                    Turn::Ask {
                        request: FsRequest::Close(file.close()),
                        next: AtomicWrite::ClosingOnError { err: be.error },
                    }
                }
                (AtomicWrite::Flushing { dir, target }, FsAnswer::Flushed(Ok((file, proof)))) => {
                    Turn::Ask {
                        request: FsRequest::Close(file.close()),
                        next: AtomicWrite::ClosingBeforeRename {
                            dir,
                            target,
                            proof: Some(proof),
                            unsynced_from: None,
                        },
                    }
                }
                (AtomicWrite::Flushing { .. }, FsAnswer::Flushed(Err((file, err)))) => Turn::Ask {
                    request: FsRequest::Close(file.close()),
                    next: AtomicWrite::ClosingOnError { err },
                },
                (
                    AtomicWrite::ClosingBeforeRename { dir, target, proof, unsynced_from },
                    FsAnswer::Closed(Ok(())),
                ) => {
                    if let Some(flushed) = proof {
                        let req = dir.rename_flushed(RenameFlushedOpts { flushed, to: target });
                        Turn::Ask {
                            request: FsRequest::RenameFlushed(req),
                            next: AtomicWrite::Renaming { dir, sync_dir: true },
                        }
                    } else {
                        let from = unsynced_from.expect("planted path");
                        let req = dir.rename_unsynced(RenameUnsyncedOpts { from, to: target });
                        Turn::Ask {
                            request: FsRequest::RenameUnsynced(req),
                            next: AtomicWrite::Renaming { dir, sync_dir: false },
                        }
                    }
                }
                (AtomicWrite::ClosingBeforeRename { .. }, FsAnswer::Closed(Err(e))) => Turn::Fail(e),
                (AtomicWrite::ClosingOnError { err }, FsAnswer::Closed(_)) => Turn::Fail(err),
                (AtomicWrite::Renaming { dir, sync_dir }, FsAnswer::Renamed(Ok(()))) => {
                    if sync_dir {
                        Turn::Ask {
                            request: FsRequest::SyncDir(dir.sync_dir()),
                            next: AtomicWrite::SyncingDir,
                        }
                    } else {
                        Turn::Done(())
                    }
                }
                (AtomicWrite::Renaming { .. }, FsAnswer::Renamed(Err(e))) => Turn::Fail(e),
                (AtomicWrite::SyncingDir, FsAnswer::SyncedDir(Ok(()))) => Turn::Done(()),
                (AtomicWrite::SyncingDir, FsAnswer::SyncedDir(Err(e))) => Turn::Fail(e),
                _ => unreachable!("typed answer matches active waiting state"),
            }
        }
    }

    fn read_file_direct(sim: &Sim, path: &FileName) -> Option<std::vec::Vec<u8>> {
        sim.exists(path).then(|| sim.contents(path))
    }

    /// The invariant: every committed `AtomicWrite` survives any subsequent `Sim::crash()` intact
    /// (byte-for-byte equal to the committed payload), and every failed or crashed uncommitted write
    /// leaves either the previous durable payload or absent—never torn bytes, and zero leaked
    /// handles on normal exit.
    fn run_durability_dst(plant: DurabilityPlant) {
        let h = heap();
        blaze_sim::check(Config::cases(120), |tape| {
            let seed = tape.next_u64("sim seed");
            let fail_rate = if tape.coin("faults") { 5 } else { 0 };
            let mut sim = Sim::new(seed, Faults { fail_one_in: fail_rate, ..Faults::default() });
            let tmp = FileName::new("state.tmp").unwrap();
            let target = FileName::new("state.bin").unwrap();

            let mut last_committed: Option<std::vec::Vec<u8>> = None;
            let rounds = 1 + (tape.below(4, "rounds") as usize);
            let mut verdict: Result<(), Broke> = Ok(());

            for r in 0..rounds {
                // Use > 2 sectors (1536 bytes) so an unsynced crash tears sectors visibly.
                let byte = (0xA0 + r as u8) as u8;
                let payload = std::vec![byte; 1536];
                with_brand(|brand| {
                    let mut handler = SimHandler::new(&mut sim, TaskId(1), brand, h.alloc());
                    let root = handler.root();
                    let buf = DmaBuf::from_slice_in(&payload, h.alloc());
                    let machine = AtomicWrite::Start {
                        dir: root,
                        tmp: tmp.clone(),
                        target: target.clone(),
                        buf,
                        plant,
                    };
                    let res = drive(&mut handler, &mut (), machine, DriveOpts { max_steps: 16 });
                    // Normal exit must have ZERO leaked handles.
                    if let Err(e) = handler.finish_task(ExitKind::Normal) {
                        verdict = Err(Broke::Oracle(format!("handle leak on normal exit: {e}")));
                        return;
                    }
                    if res.is_ok() {
                        last_committed = Some(payload.clone());
                    }
                });
                if verdict.is_err() {
                    return verdict;
                }

                // Cut power after each round!
                sim.crash();
                let on_disk = read_file_direct(&sim, &target);
                if last_committed.as_ref() == Some(&payload) {
                    // Round committed: disk MUST hold the exact new payload, never torn or rolled back.
                    if on_disk.as_ref() != Some(&payload) {
                        return Err(Broke::Oracle(format!(
                            "durability lost after crash in round {r}: expected {} bytes of {:#x}, got {:?}",
                            payload.len(),
                            payload[0],
                            on_disk.as_ref().map(|v| (v.len(), v.first().copied()))
                        )));
                    }
                } else {
                    // Round failed (e.g. injected fault during Write, Flush, Rename, or trailing SyncDir):
                    // disk must hold EITHER the previous durable payload OR the full untorn new payload
                    // (if Rename already landed before a trailing SyncDir fault)—NEVER torn bytes.
                    let matches_prev = on_disk == last_committed;
                    let matches_full_new = on_disk.as_ref() == Some(&payload);
                    if !matches_prev && !matches_full_new {
                        return Err(Broke::Oracle(format!(
                            "torn or corrupted state after failed round {r} crash: got {:?}",
                            on_disk.as_ref().map(|v| (v.len(), v.first().copied()))
                        )));
                    }
                    if matches_full_new {
                        last_committed = Some(payload);
                    }
                }
            }
            Ok(())
        });
    }

    #[test]
    fn atomic_write_typestate_durability_dst() {
        run_durability_dst(DurabilityPlant::None);
    }

    #[test]
    #[should_panic(expected = "durability lost after crash")]
    fn the_oracle_catches_unflushed_rename_on_crash() {
        run_durability_dst(DurabilityPlant::SkipFlushAndSync);
    }

    #[test]
    fn absent_file_is_not_found_and_lent_read_enforces_cap_and_leak_fails_loudly() {
        let h = heap();
        let mut sim = Sim::new(7, Faults::default());
        let missing = FileName::new("does-not-exist.txt").unwrap();
        let existing = FileName::new("hello.txt").unwrap();

        with_brand(|brand| {
            let mut handler = SimHandler::new(&mut sim, TaskId(42), brand, h.alloc());
            let ro = handler.root().narrow_read_only();

            // 1. ReadAll on missing file returns FsError::NotFound, never empty!
            let req = FsRequest::ReadAll(ro.read_all(ReadAllOpts {
                path: missing.clone(),
                max: MaxBytes(1024),
                buf: DmaBuf::with_capacity_in(ByteLen(64), h.alloc()),
            }));
            use blaze_machine::Handler as _;
            let ans = handler.perform(req, |p| match p {
                blaze_machine::Perform::Answered(FsAnswer::ReadAll(res)) => res,
                _ => unreachable!(),
            }).unwrap();
            match ans {
                Err(be) => {
                    assert_eq!(be.error, FsError::NotFound { path: missing.clone() });
                    be.buf.release();
                }
                Ok(_) => panic!("missing file must return NotFound, not empty"),
            }

            // 2. Create a file, leave it open, verify capped Read rejects > LENT_READ_MAX,
            //    and verify normal exit reports LeakedHandles loudly!
            let open_req = FsRequest::OpenWrite(handler.root().open_write(OpenOpts {
                path: existing.clone(),
                create: true,
                truncate: true,
            }));
            let file = handler.perform(open_req, |p| match p {
                blaze_machine::Perform::Answered(FsAnswer::OpenedWrite(Ok(f))) => f,
                _ => unreachable!(),
            }).unwrap();

            let too_big = FsRequest::Read(file.read(ReadOpts {
                at: ByteOffset(0),
                len: ByteLen(LENT_READ_MAX.0 + 1),
            }));
            let err = handler.perform(too_big, |p| match p {
                blaze_machine::Perform::Answered(FsAnswer::Read(Err(e))) => e,
                _ => unreachable!(),
            }).unwrap();
            assert_eq!(
                err,
                FsError::LimitExceeded {
                    requested: u64::from(LENT_READ_MAX.0) + 1,
                    max: u64::from(LENT_READ_MAX.0),
                }
            );

            // Deliberately forget to close `file` (raw id leak test) -> normal exit MUST fail loudly!
            let _leaked_id = file.into_id();
            assert!(matches!(
                handler.finish_task(ExitKind::Normal),
                Err(SimHandlerError::LeakedHandles(l)) if l.task == TaskId(42) && l.count == 1
            ));
        });
    }
}
