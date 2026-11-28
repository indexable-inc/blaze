//! Zero-alloc typed linear requests (`blaze_machine::Request`), strong newtypes (`ByteOffset`,
//! `ByteLen`, `MaxBytes`), `#[blaze::args]` `*Opts` builders, owned DMA buffers (`DmaBuf`), and
//! generic [`DriverHandler<'io, 'd, D>`] driving machines over any [`crate::Driver`] (`Native`,
//! `Threads`, `Kqueue`, `Uring`, or `Sim`)
//! (`docs/v2/plan/design/sans-io-machines.md`, sections 3–4).
//!
//! Key guarantees:
//! - Every DMA request (`ReadInto`, `WriteFrom`, `ReadAll`, `WriteDurable`, `SendFrom`,
//!   `RecvInto`, `FillRandom`) takes an owned caller [`DmaBuf`] and returns it in its answer
//!   (`FilledBuf`, `DmaBuf`, or `BufError`); the driver never allocates a `Vec<u8>` on the global
//!   heap for a caller.
//! - Lent-slice [`Read`] is capped to [`LENT_READ_MAX`] (backed by a fixed driver scratch buffer).
//! - Absent files return [`FsError::NotFound`], never an empty byte slice.
//! - Typestate durability: writable files open as `File<'io, Unflushed>`, `Flush` consumes
//!   `Unflushed` and returns `File<'io, Flushed>` in its answer, and `RenameFlushed` requires a
//!   `FlushedProof` minted only by `Flush`—making durability ordering checkable in types.

mod handler;
mod net;
#[cfg(test)]
mod tests;

use core::fmt;
use core::time::Duration;

use blaze_machine::Request;
use blaze_own::{Alloc, Vec};

use crate::handle::{
    Dir, File, FileId, Flushed, HandleError, Lock as LockHandle, LockId, ReadOnly, ReadWrite,
    Unflushed,
};
use crate::{DirName, FileName, Listing, LockMode, Meta};

pub use handler::{DriverHandler, DriverHandlerError, SimHandler, SimHandlerError};
pub use net::{
    Accept, CloseConn, Connect, ConnectOpts, Listen, ListenOpts, NetError, RecvInto, RecvIntoOpts,
    SendFrom, SendFromOpts, Unlisten,
};

// ---------------------------------------------------------------------------
// Strong newtypes (rule 73) and owned DMA buffers
// ---------------------------------------------------------------------------

/// Byte offset inside a file (or byte count copied).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct ByteOffset(pub u64);

/// Exact byte length of one IO transfer.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Debug)]
pub struct ByteLen(pub u32);

/// Explicit upper byte cap for variable-size reads (`ReadAll`, rule 18 `bounded-progress`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct MaxBytes(pub u64);

/// Registered `io_uring` fixed-buffer index (`IORING_REGISTER_BUFFERS`, rule 73).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FixedBufIndex(pub u16);

/// Hard upper bound for a lent-slice [`Read`] served from the driver's per-worker scratch buffer.
pub const LENT_READ_MAX: ByteLen = ByteLen(64 * 1024);

/// Caller-owned DMA buffer backed by an explicit caller allocator (`blaze_own::Vec`), moved into
/// a request and returned in its answer (zero global heap allocation).
#[blaze::linear]
#[derive(Debug)]
pub struct DmaBuf<'a> {
    pub(crate) bytes: Vec<u8, Alloc<'a>>,
    buf_index: Option<FixedBufIndex>,
}

impl<'a> DmaBuf<'a> {
    /// Allocates an empty DMA buffer with capacity `cap` in `alloc`.
    #[inline]
    pub fn with_capacity_in(cap: ByteLen, alloc: Alloc<'a>) -> Self {
        Self { bytes: Vec::with_capacity_in(cap.0 as usize, alloc), buf_index: None }
    }

    /// Creates a DMA buffer initialized with `data` copied into `alloc`.
    #[inline]
    pub fn from_slice_in(data: &[u8], alloc: Alloc<'a>) -> Self {
        let mut bytes = Vec::with_capacity_in(data.len(), alloc);
        bytes.extend_from_slice(data);
        Self { bytes, buf_index: None }
    }

    /// Tags this DMA buffer with a registered `io_uring` fixed-buffer index.
    #[blaze::consumes]
    #[inline]
    pub fn with_registered_index(mut self, index: FixedBufIndex) -> Self {
        self.buf_index = Some(index);
        self
    }

    /// Returns the registered `io_uring` fixed-buffer index, if set.
    #[inline]
    pub const fn registered_index(&self) -> Option<FixedBufIndex> {
        self.buf_index
    }

    /// Returns `true` if this DMA buffer is tagged with a registered `io_uring` fixed-buffer index.
    #[inline]
    pub const fn is_registered(&self) -> bool {
        self.buf_index.is_some()
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
    pub fn capacity(&self) -> usize {
        self.bytes.capacity()
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
        let Self { bytes, buf_index: _ } = self;
        drop(bytes);
    }
}

/// Filled DMA buffer returned by `ReadInto` / `ReadAll` / `RecvInto`.
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
                write!(f, "length {requested} exceeds cap {max}")
            }
            FsError::NoSpace { path } => write!(f, "no space left writing {path}"),
            FsError::StaleHandle(e) => write!(f, "stale handle: {e}"),
            FsError::Io { path: Some(p), message } => write!(f, "io error on {p}: {message}"),
            FsError::Io { path: None, message } => write!(f, "io error: {message}"),
        }
    }
}

/// Proof token minted only when [`Flush`] completes durably for `path` (rule 74 `verify-once`).
/// Required by [`RenameFlushed`] so forgetting `fsync` before `rename` is a compile error.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FlushedProof {
    pub(crate) path: FileName,
}

impl FlushedProof {
    #[inline]
    pub fn path(&self) -> &FileName {
        &self.path
    }
}

/// Outcome of a non-blocking advisory file lock request ([`Lock`]).
#[derive(Debug)]
pub enum LockOutcome<'io> {
    /// Lock acquired; the caller must consume it via `lock.unlock()`.
    Acquired(LockHandle<'io>),
    /// Another holder holds an incompatible lock; nothing is held.
    Contended,
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

/// Options for listing direct entries of a directory (`Effect::Entries`).
#[blaze::args]
pub struct ListOpts {
    pub dir: DirName,
}

/// Options for truncating an open writable file to `len` bytes.
#[blaze::args]
pub struct TruncateOpts {
    pub len: ByteOffset,
}

/// Options for acquiring a non-blocking advisory file lock (`Effect::Lock`).
#[blaze::args]
pub struct LockOpts {
    pub path: FileName,
    pub mode: LockMode,
}

/// Options for kernel-level sparse data copy (`Effect::CopyData`).
#[blaze::args]
pub struct CopyDataOpts {
    pub from: FileName,
    pub to: FileName,
}

/// Options for a durable atomic whole-file write (`Effect::Write`).
#[blaze::args]
pub struct WriteDurableOpts<'a> {
    pub path: FileName,
    pub buf: DmaBuf<'a>,
}

/// Options for sleeping on the driver's monotonic/virtual clock (`Effect::Timer`).
#[blaze::args]
pub struct SleepOpts {
    pub after: Duration,
}

/// Options for filling a caller-owned DMA buffer with secure random bytes (`Effect::Random`).
#[blaze::args]
pub struct FillRandomOpts<'a> {
    pub buf: DmaBuf<'a>,
}

// ---------------------------------------------------------------------------
// Typed linear filesystem & host requests implementing `blaze_machine::Request`
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

#[blaze::linear]
#[derive(Debug)]
pub struct Stat<'io> {
    pub(crate) dir: Dir<'io, ReadOnly>,
    pub(crate) path: FileName,
}

impl<'io> Request for Stat<'io> {
    type Answer<'s> = Result<Option<Meta>, FsError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct List<'io> {
    pub(crate) scope: Dir<'io, ReadOnly>,
    pub(crate) dir: DirName,
}

impl<'io> Request for List<'io> {
    type Answer<'s> = Result<Listing, FsError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct Remove<'io> {
    pub(crate) dir: Dir<'io, ReadWrite>,
    pub(crate) path: FileName,
}

impl<'io> Request for Remove<'io> {
    type Answer<'s> = Result<(), FsError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct MakeDir<'io> {
    pub(crate) dir: Dir<'io, ReadWrite>,
    pub(crate) path: FileName,
}

impl<'io> Request for MakeDir<'io> {
    type Answer<'s> = Result<(), FsError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct RemoveDir<'io> {
    pub(crate) dir: Dir<'io, ReadWrite>,
    pub(crate) path: FileName,
}

impl<'io> Request for RemoveDir<'io> {
    type Answer<'s> = Result<(), FsError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct Truncate<'io> {
    pub(crate) file: FileId<'io>,
    pub(crate) len: ByteOffset,
}

impl<'io> Request for Truncate<'io> {
    type Answer<'s> = Result<(), FsError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct Lock<'io> {
    pub(crate) dir: Dir<'io, ReadWrite>,
    pub(crate) path: FileName,
    pub(crate) mode: LockMode,
}

impl<'io> Request for Lock<'io> {
    type Answer<'s> = Result<LockOutcome<'io>, FsError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct Unlock<'io> {
    pub(crate) lock: LockId<'io>,
}

impl<'io> Request for Unlock<'io> {
    type Answer<'s> = Result<(), FsError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct CopyData<'io> {
    pub(crate) dir: Dir<'io, ReadWrite>,
    pub(crate) from: FileName,
    pub(crate) to: FileName,
}

impl<'io> Request for CopyData<'io> {
    type Answer<'s> = Result<ByteOffset, FsError>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct WriteDurable<'io, 'a> {
    pub(crate) dir: Dir<'io, ReadWrite>,
    pub(crate) path: FileName,
    pub(crate) buf: DmaBuf<'a>,
}

impl<'io, 'a> Request for WriteDurable<'io, 'a> {
    type Answer<'s> = Result<DmaBuf<'a>, BufError<'a, FsError>>;
}

#[blaze::linear]
#[derive(Debug)]
pub struct Sleep {
    pub(crate) after: Duration,
}

impl Sleep {
    #[inline]
    pub fn new(opts: SleepOpts) -> Self {
        Self { after: opts.after }
    }
}

impl Request for Sleep {
    type Answer<'s> = ();
}

#[blaze::linear]
#[derive(Debug)]
pub struct FillRandom<'a> {
    pub(crate) buf: DmaBuf<'a>,
}

impl<'a> FillRandom<'a> {
    #[inline]
    pub fn new(opts: FillRandomOpts<'a>) -> Self {
        Self { buf: opts.buf }
    }
}

impl<'a> Request for FillRandom<'a> {
    type Answer<'s> = Result<DmaBuf<'a>, BufError<'a, FsError>>;
}

// ---------------------------------------------------------------------------
// Handle methods that build inert requests (take no `cx`, never wait)
// ---------------------------------------------------------------------------

impl<'io, Mode> Dir<'io, Mode> {
    #[inline]
    pub fn stat(&self, path: FileName) -> Stat<'io> {
        Stat { dir: self.narrow_read_only(), path }
    }

    #[inline]
    pub fn list(&self, opts: ListOpts) -> List<'io> {
        List { scope: self.narrow_read_only(), dir: opts.dir }
    }
}

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

    #[inline]
    pub fn remove(&self, path: FileName) -> Remove<'io> {
        Remove { dir: *self, path }
    }

    #[inline]
    pub fn make_dir(&self, path: FileName) -> MakeDir<'io> {
        MakeDir { dir: *self, path }
    }

    #[inline]
    pub fn remove_dir(&self, path: FileName) -> RemoveDir<'io> {
        RemoveDir { dir: *self, path }
    }

    #[inline]
    pub fn lock(&self, opts: LockOpts) -> Lock<'io> {
        Lock { dir: *self, path: opts.path, mode: opts.mode }
    }

    #[inline]
    pub fn copy_data(&self, opts: CopyDataOpts) -> CopyData<'io> {
        CopyData { dir: *self, from: opts.from, to: opts.to }
    }

    #[inline]
    pub fn write_durable<'a>(&self, opts: WriteDurableOpts<'a>) -> WriteDurable<'io, 'a> {
        WriteDurable { dir: *self, path: opts.path, buf: opts.buf }
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

    #[inline]
    pub fn truncate(&self, opts: TruncateOpts) -> Truncate<'io> {
        Truncate { file: self.id(), len: opts.len }
    }

    /// Consumes `File<'io, Unflushed>` into a [`Flush`] request that mints `File<'io, Flushed>`
    /// and a [`FlushedProof`] on success.
    #[blaze::consumes]
    #[inline]
    pub fn flush(self) -> Flush<'io> {
        Flush { file: self }
    }
}

impl<'io> LockHandle<'io> {
    /// Consumes this linear advisory file lock into an [`Unlock`] request.
    #[blaze::consumes]
    #[inline]
    pub fn unlock(self) -> Unlock<'io> {
        Unlock { lock: self.into_id() }
    }
}

// ---------------------------------------------------------------------------
// Unified request/answer unions for Rust-edition machines & `DriverHandler`
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
    Stat(Stat<'io>),
    List(List<'io>),
    Remove(Remove<'io>),
    MakeDir(MakeDir<'io>),
    RemoveDir(RemoveDir<'io>),
    Truncate(Truncate<'io>),
    Lock(Lock<'io>),
    Unlock(Unlock<'io>),
    CopyData(CopyData<'io>),
    WriteDurable(WriteDurable<'io, 'a>),
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
    Stat(Result<Option<Meta>, FsError>),
    Listed(Result<Listing, FsError>),
    Removed(Result<(), FsError>),
    MadeDir(Result<(), FsError>),
    RemovedDir(Result<(), FsError>),
    Truncated(Result<(), FsError>),
    Locked(Result<LockOutcome<'io>, FsError>),
    Unlocked(Result<(), FsError>),
    CopiedData(Result<ByteOffset, FsError>),
    WrittenDurable(Result<DmaBuf<'a>, BufError<'a, FsError>>),
}

impl<'io, 'a> Request for FsRequest<'io, 'a> {
    type Answer<'s> = FsAnswer<'io, 'a, 's>;
}

/// Unified typed IO request union spanning filesystem, network, clock, and randomness.
#[blaze::linear]
#[derive(Debug)]
pub enum IoRequest<'io, 'a> {
    // Filesystem
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
    Stat(Stat<'io>),
    List(List<'io>),
    Remove(Remove<'io>),
    MakeDir(MakeDir<'io>),
    RemoveDir(RemoveDir<'io>),
    Truncate(Truncate<'io>),
    Lock(Lock<'io>),
    Unlock(Unlock<'io>),
    CopyData(CopyData<'io>),
    WriteDurable(WriteDurable<'io, 'a>),
    // Network
    Listen(Listen<'io>),
    Accept(Accept<'io>),
    Connect(Connect<'io>),
    SendFrom(SendFrom<'io, 'a>),
    RecvInto(RecvInto<'io, 'a>),
    CloseConn(CloseConn<'io>),
    Unlisten(Unlisten<'io>),
    // Time & Random
    Sleep(Sleep),
    FillRandom(FillRandom<'a>),
}

/// Unified typed IO answer matching [`IoRequest`].
#[derive(Debug)]
pub enum IoAnswer<'io, 'a, 's> {
    // Filesystem
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
    Stat(Result<Option<Meta>, FsError>),
    Listed(Result<Listing, FsError>),
    Removed(Result<(), FsError>),
    MadeDir(Result<(), FsError>),
    RemovedDir(Result<(), FsError>),
    Truncated(Result<(), FsError>),
    Locked(Result<LockOutcome<'io>, FsError>),
    Unlocked(Result<(), FsError>),
    CopiedData(Result<ByteOffset, FsError>),
    WrittenDurable(Result<DmaBuf<'a>, BufError<'a, FsError>>),
    // Network
    Listened(Result<crate::handle::Listener<'io>, NetError>),
    Accepted(Result<crate::handle::Conn<'io>, NetError>),
    Connected(Result<crate::handle::Conn<'io>, NetError>),
    Sent(Result<DmaBuf<'a>, BufError<'a, NetError>>),
    Recved(Result<FilledBuf<'a>, BufError<'a, NetError>>),
    ClosedConn(Result<(), NetError>),
    Unlistened(Result<(), NetError>),
    // Time & Random
    Slept,
    FilledRandom(Result<DmaBuf<'a>, BufError<'a, FsError>>),
}

impl<'io, 'a> Request for IoRequest<'io, 'a> {
    type Answer<'s> = IoAnswer<'io, 'a, 's>;
}

impl<'io, 'a> From<FsRequest<'io, 'a>> for IoRequest<'io, 'a> {
    #[inline]
    fn from(req: FsRequest<'io, 'a>) -> Self {
        match req {
            FsRequest::OpenWrite(r) => IoRequest::OpenWrite(r),
            FsRequest::OpenRead(r) => IoRequest::OpenRead(r),
            FsRequest::Close(r) => IoRequest::Close(r),
            FsRequest::Read(r) => IoRequest::Read(r),
            FsRequest::ReadInto(r) => IoRequest::ReadInto(r),
            FsRequest::WriteFrom(r) => IoRequest::WriteFrom(r),
            FsRequest::Flush(r) => IoRequest::Flush(r),
            FsRequest::ReadAll(r) => IoRequest::ReadAll(r),
            FsRequest::RenameFlushed(r) => IoRequest::RenameFlushed(r),
            FsRequest::RenameUnsynced(r) => IoRequest::RenameUnsynced(r),
            FsRequest::SyncDir(r) => IoRequest::SyncDir(r),
            FsRequest::Stat(r) => IoRequest::Stat(r),
            FsRequest::List(r) => IoRequest::List(r),
            FsRequest::Remove(r) => IoRequest::Remove(r),
            FsRequest::MakeDir(r) => IoRequest::MakeDir(r),
            FsRequest::RemoveDir(r) => IoRequest::RemoveDir(r),
            FsRequest::Truncate(r) => IoRequest::Truncate(r),
            FsRequest::Lock(r) => IoRequest::Lock(r),
            FsRequest::Unlock(r) => IoRequest::Unlock(r),
            FsRequest::CopyData(r) => IoRequest::CopyData(r),
            FsRequest::WriteDurable(r) => IoRequest::WriteDurable(r),
        }
    }
}
