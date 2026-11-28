//! Zero-`Effect`/`Done`-bridge native `io_uring` SQE planner ([`UringSqePlan`]) and direct
//! filesystem handler seam ([`DirectFsHandler<'io, 'b, B>`])
//! (`docs/v2/plan/design/sans-io-machines.md`).
//!
//! This module proves two structural properties without touching the legacy `Effect`/`Done` enum:
//! 1. [`UringSqePlan`] lowers typed linear requests (`ReadInto`, `WriteFrom`, `Flush`,
//!    `Chain<WriteFrom, Flush>`, `Batch<Stat, CAP>`) directly into fixed-file, fixed-buffer,
//!    `IOSQE_IO_LINK`-chained SQE descriptors in pure sans-IO Rust (compiling and testing on both
//!    macOS and Linux).
//! 2. [`DirectFsHandler`] implements `blaze_machine::Handler<R>` directly over any
//!    [`DirectFsBackend`] (such as the crash-simulable [`InMemoryDirectFs`]) with generational
//!    handle tables, zero `Effect`/`Done` translation, and small non-closure-generic execution
//!    frames.

use core::fmt;
use core::ops::{BitAnd, BitOr};

use blaze_machine::{Batch, BatchReady, Chain, ChainOutcome, Handler, Perform};
use blaze_own::planted::{NoPlant, Planted, Plants};
use blaze_own::{Alloc, Brand, FixedVec, Vec};

use crate::handle::{
    Dir, DirKind, ExitKind, File, FileKind, FixedFileSlot, Flushed, HandleError, HandleTable,
    HandleTableOpts, LeakedHandles, ReadOnly, ReadWrite, TaskId, Unflushed,
};
use crate::request::{
    BufError, ByteLen, Close, DmaBuf, FilledBuf, FixedBufIndex, Flush, FlushedProof, FsError,
    OpenRead, OpenWrite, ReadInto, RenameFlushed, Stat, SyncDir, WriteFrom,
};
use crate::{DirName, FileName, Meta};

// ---------------------------------------------------------------------------
// 1. Pure sans-IO `io_uring` SQE lowering planner (`UringSqePlan`)
// ---------------------------------------------------------------------------

/// `io_uring` submission queue entry opcode targeted by a typed linear request.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SqeOpcode {
    OpenAt,
    Read,
    ReadFixed,
    Write,
    WriteFixed,
    Fsync,
    Close,
    RenameAt,
    Statx,
    Connect,
    Accept,
    Send,
    Recv,
    PollAdd,
}

/// Bitflags carried on a [`PlannedSqe`] (`IOSQE_FIXED_FILE`, `IOSQE_IO_LINK`,
/// `IOSQE_BUFFER_SELECT`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub struct SqeFlags(pub u8);

impl SqeFlags {
    pub const NONE: Self = Self(0);
    /// `IOSQE_FIXED_FILE`: target `fd_slot` is a registered fixed-file table index.
    pub const FIXED_FILE: Self = Self(1 << 0);
    /// `IOSQE_IO_LINK`: link this SQE sequentially to the next SQE in the submission ring.
    pub const IO_LINK: Self = Self(1 << 1);
    /// `IOSQE_BUFFER_SELECT`: select a registered buffer group on completion.
    pub const BUFFER_SELECT: Self = Self(1 << 2);

    #[inline]
    pub const fn bits(self) -> u8 {
        self.0
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn contains(self, flag: Self) -> bool {
        (self.0 & flag.0) == flag.0
    }

    #[inline]
    pub const fn with(self, flag: Self) -> Self {
        Self(self.0 | flag.0)
    }
}

impl BitOr for SqeFlags {
    type Output = Self;

    #[inline]
    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitAnd for SqeFlags {
    type Output = Self;

    #[inline]
    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

/// Pure, platform-independent `io_uring` SQE plan produced from a typed linear request.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct PlannedSqe {
    pub opcode: SqeOpcode,
    pub flags: SqeFlags,
    pub fd_slot: Option<FixedFileSlot>,
    pub buf_index: Option<FixedBufIndex>,
    pub offset: u64,
    pub len: u32,
}

/// Planted-bug switches for `UringSqePlan` oracle controls (rule 43 & 78).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum SqePlanPlant {
    #[default]
    None,
    /// Planted bug: omit `IOSQE_IO_LINK` on the first SQE of a chained write+flush.
    SkipIoLinkOnChain,
    /// Planted bug: lower registered buffers as plain `Read`/`Write` instead of
    /// `ReadFixed`/`WriteFixed`.
    SkipFixedBufOpcode,
    /// Planted bug: omit `IOSQE_FIXED_FILE` on fixed-file handle SQEs.
    SkipFixedFileFlag,
}

impl Plants for SqePlanPlant {}

/// Pure sans-IO `io_uring` SQE planner namespace (also exposed as top-level `plan_*` functions).
#[derive(Clone, Copy, Default, Debug)]
pub struct UringSqePlan;

impl UringSqePlan {
    #[inline]
    pub fn plan_read_into(req: &ReadInto<'_, '_>) -> PlannedSqe {
        plan_read_into(req)
    }

    #[inline]
    pub fn plan_read_into_with<P: Planted<SqePlanPlant>>(
        req: &ReadInto<'_, '_>,
        plant: P,
    ) -> PlannedSqe {
        plan_read_into_with(req, plant)
    }

    #[inline]
    pub fn plan_write_from(req: &WriteFrom<'_, '_>) -> PlannedSqe {
        plan_write_from(req)
    }

    #[inline]
    pub fn plan_write_from_with<P: Planted<SqePlanPlant>>(
        req: &WriteFrom<'_, '_>,
        plant: P,
    ) -> PlannedSqe {
        plan_write_from_with(req, plant)
    }

    #[inline]
    pub fn plan_flush(req: &Flush<'_>) -> PlannedSqe {
        plan_flush(req)
    }

    #[inline]
    pub fn plan_flush_with<P: Planted<SqePlanPlant>>(req: &Flush<'_>, plant: P) -> PlannedSqe {
        plan_flush_with(req, plant)
    }

    #[inline]
    pub fn plan_chain_write_flush(req: &Chain<WriteFrom<'_, '_>, Flush<'_>>) -> [PlannedSqe; 2] {
        plan_chain_write_flush(req)
    }

    #[inline]
    pub fn plan_chain_write_flush_with<P: Planted<SqePlanPlant>>(
        req: &Chain<WriteFrom<'_, '_>, Flush<'_>>,
        plant: P,
    ) -> [PlannedSqe; 2] {
        plan_chain_write_flush_with(req, plant)
    }

    #[inline]
    pub fn plan_stat(req: &Stat<'_>) -> PlannedSqe {
        plan_stat(req)
    }

    #[inline]
    pub fn plan_stat_with<P: Planted<SqePlanPlant>>(req: &Stat<'_>, plant: P) -> PlannedSqe {
        plan_stat_with(req, plant)
    }

    #[inline]
    pub fn plan_batch_stat<const CAP: usize>(
        batch: &Batch<Stat<'_>, CAP>,
    ) -> FixedVec<PlannedSqe, CAP> {
        plan_batch_stat(batch)
    }

    #[inline]
    pub fn plan_batch_stat_with<const CAP: usize, P: Planted<SqePlanPlant>>(
        batch: &Batch<Stat<'_>, CAP>,
        plant: P,
    ) -> FixedVec<PlannedSqe, CAP> {
        plan_batch_stat_with(batch, plant)
    }
}

/// Lowers [`ReadInto`] into a [`PlannedSqe`], selecting `ReadFixed` when the DMA buffer carries
/// a registered [`FixedBufIndex`] and setting `IOSQE_FIXED_FILE` for the file's slot.
#[inline]
pub fn plan_read_into(req: &ReadInto<'_, '_>) -> PlannedSqe {
    plan_read_into_with(req, NoPlant)
}

/// Plant-parameterized version of [`plan_read_into`].
#[inline]
pub fn plan_read_into_with<P: Planted<SqePlanPlant>>(
    req: &ReadInto<'_, '_>,
    plant: P,
) -> PlannedSqe {
    let want = if !req.buf.is_empty() { req.buf.len() } else { req.buf.capacity() };
    let len = u32::try_from(want).unwrap_or(u32::MAX);
    let (opcode, buf_index) = match req.buf.registered_index() {
        Some(idx) if !plant.has(SqePlanPlant::SkipFixedBufOpcode) => {
            (SqeOpcode::ReadFixed, Some(idx))
        }
        _ => (SqeOpcode::Read, None),
    };
    let flags =
        if plant.has(SqePlanPlant::SkipFixedFileFlag) { SqeFlags::NONE } else { SqeFlags::FIXED_FILE };
    PlannedSqe {
        opcode,
        flags,
        fd_slot: Some(req.file.fixed_file_slot()),
        buf_index,
        offset: req.at.0,
        len,
    }
}

/// Lowers [`WriteFrom`] into a [`PlannedSqe`], selecting `WriteFixed` when the DMA buffer carries
/// a registered [`FixedBufIndex`] and setting `IOSQE_FIXED_FILE` for the file's slot.
#[inline]
pub fn plan_write_from(req: &WriteFrom<'_, '_>) -> PlannedSqe {
    plan_write_from_with(req, NoPlant)
}

/// Plant-parameterized version of [`plan_write_from`].
#[inline]
pub fn plan_write_from_with<P: Planted<SqePlanPlant>>(
    req: &WriteFrom<'_, '_>,
    plant: P,
) -> PlannedSqe {
    let len = u32::try_from(req.buf.len()).unwrap_or(u32::MAX);
    let (opcode, buf_index) = match req.buf.registered_index() {
        Some(idx) if !plant.has(SqePlanPlant::SkipFixedBufOpcode) => {
            (SqeOpcode::WriteFixed, Some(idx))
        }
        _ => (SqeOpcode::Write, None),
    };
    let flags =
        if plant.has(SqePlanPlant::SkipFixedFileFlag) { SqeFlags::NONE } else { SqeFlags::FIXED_FILE };
    PlannedSqe {
        opcode,
        flags,
        fd_slot: Some(req.file.fixed_file_slot()),
        buf_index,
        offset: req.at.0,
        len,
    }
}

/// Lowers [`Flush`] into an `Fsync` [`PlannedSqe`] targeting the file's registered fixed-file slot.
#[inline]
pub fn plan_flush(req: &Flush<'_>) -> PlannedSqe {
    plan_flush_with(req, NoPlant)
}

/// Plant-parameterized version of [`plan_flush`].
#[inline]
pub fn plan_flush_with<P: Planted<SqePlanPlant>>(req: &Flush<'_>, plant: P) -> PlannedSqe {
    let flags =
        if plant.has(SqePlanPlant::SkipFixedFileFlag) { SqeFlags::NONE } else { SqeFlags::FIXED_FILE };
    PlannedSqe {
        opcode: SqeOpcode::Fsync,
        flags,
        fd_slot: Some(req.file.id().fixed_file_slot()),
        buf_index: None,
        offset: 0,
        len: 0,
    }
}

/// Lowers `Chain<WriteFrom, Flush>` into a linked 2-SQE submission with `IOSQE_IO_LINK` set on
/// the leading write SQE.
#[inline]
pub fn plan_chain_write_flush(req: &Chain<WriteFrom<'_, '_>, Flush<'_>>) -> [PlannedSqe; 2] {
    plan_chain_write_flush_with(req, NoPlant)
}

/// Plant-parameterized version of [`plan_chain_write_flush`].
#[inline]
pub fn plan_chain_write_flush_with<P: Planted<SqePlanPlant>>(
    req: &Chain<WriteFrom<'_, '_>, Flush<'_>>,
    plant: P,
) -> [PlannedSqe; 2] {
    let mut first = plan_write_from_with(&req.first, plant);
    if !plant.has(SqePlanPlant::SkipIoLinkOnChain) {
        first.flags = first.flags.with(SqeFlags::IO_LINK);
    }
    let second = plan_flush_with(&req.second, plant);
    [first, second]
}

/// Lowers a single [`Stat`] request into a `Statx` [`PlannedSqe`] anchored at its directory slot.
#[inline]
pub fn plan_stat(req: &Stat<'_>) -> PlannedSqe {
    plan_stat_with(req, NoPlant)
}

/// Plant-parameterized version of [`plan_stat`].
#[inline]
pub fn plan_stat_with<P: Planted<SqePlanPlant>>(req: &Stat<'_>, plant: P) -> PlannedSqe {
    let flags =
        if plant.has(SqePlanPlant::SkipFixedFileFlag) { SqeFlags::NONE } else { SqeFlags::FIXED_FILE };
    PlannedSqe {
        opcode: SqeOpcode::Statx,
        flags,
        fd_slot: Some(req.dir.id().fixed_file_slot()),
        buf_index: None,
        offset: 0,
        len: 0,
    }
}

/// Lowers every entry of `Batch<Stat, CAP>` into `Statx` SQEs for a single `io_uring_enter`.
pub fn plan_batch_stat<const CAP: usize>(
    batch: &Batch<Stat<'_>, CAP>,
) -> FixedVec<PlannedSqe, CAP> {
    plan_batch_stat_with(batch, NoPlant)
}

/// Plant-parameterized version of [`plan_batch_stat`].
pub fn plan_batch_stat_with<const CAP: usize, P: Planted<SqePlanPlant>>(
    batch: &Batch<Stat<'_>, CAP>,
    plant: P,
) -> FixedVec<PlannedSqe, CAP> {
    let mut out: FixedVec<PlannedSqe, CAP> = FixedVec::new();
    for stat in batch.as_slice() {
        out.push(plan_stat_with(stat, plant))
            .ok()
            .expect("batch SQE count cannot exceed CAP");
    }
    out
}

// ---------------------------------------------------------------------------
// 2. Direct zero-`Effect`/`Done` filesystem backend & `DirectFsHandler`
// ---------------------------------------------------------------------------

/// Direct filesystem execution seam consumed by [`DirectFsHandler`] with zero `Effect`/`Done`
/// enum translation.
pub trait DirectFsBackend {
    fn open_write(&mut self, path: &FileName, create: bool, truncate: bool) -> Result<(), FsError>;
    fn open_read(&mut self, path: &FileName) -> Result<(), FsError>;
    fn read_at(&mut self, path: &FileName, at: u64, dst: &mut [u8]) -> Result<usize, FsError>;
    fn write_at(&mut self, path: &FileName, at: u64, src: &[u8]) -> Result<(), FsError>;
    fn flush(&mut self, path: &FileName) -> Result<(), FsError>;
    fn rename(&mut self, from: &FileName, to: &FileName) -> Result<(), FsError>;
    fn sync_dir(&mut self, dir: &DirName) -> Result<(), FsError>;
    fn stat(&mut self, path: &FileName) -> Result<Option<Meta>, FsError>;
}

struct MemFile<'a> {
    path: FileName,
    ino: u64,
    live_bytes: Vec<u8, Alloc<'a>>,
    flushed_bytes: Vec<u8, Alloc<'a>>,
    modified_ns: u64,
    changed_ns: u64,
}

struct DurableFile<'a> {
    path: FileName,
    ino: u64,
    bytes: Vec<u8, Alloc<'a>>,
    modified_ns: u64,
    changed_ns: u64,
}

/// Deterministic in-memory [`DirectFsBackend`] backed by an explicit caller [`Alloc<'a>`] with
/// power-cut crash simulation:
/// - Writes land in volatile memory until [`DirectFsBackend::flush`] persists the file's bytes.
/// - Creates and renames stay volatile until [`DirectFsBackend::sync_dir`] persists directory
///   entries (snapshotting each file's flushed bytes, never unflushed bytes).
/// - [`InMemoryDirectFs::crash`] drops every unsynced name and every unflushed byte.
pub struct InMemoryDirectFs<'a> {
    alloc: Alloc<'a>,
    live: Vec<MemFile<'a>, Alloc<'a>>,
    durable: Vec<DurableFile<'a>, Alloc<'a>>,
    next_ino: u64,
    tick: u64,
}

impl<'a> InMemoryDirectFs<'a> {
    /// Creates an empty crash-simulable in-memory filesystem in `alloc`.
    pub fn new(alloc: Alloc<'a>) -> Self {
        Self {
            alloc,
            live: Vec::new_in(alloc),
            durable: Vec::new_in(alloc),
            next_ino: 1,
            tick: 1,
        }
    }

    /// Simulates a sudden power loss: discards all volatile state and restores only flushed bytes
    /// whose names were made durable via [`DirectFsBackend::sync_dir`].
    pub fn crash(&mut self) {
        self.live.clear();
        for entry in self.durable.as_slice() {
            self.live.push(MemFile {
                path: entry.path.clone(),
                ino: entry.ino,
                live_bytes: Vec::from_slice_in(entry.bytes.as_slice(), self.alloc),
                flushed_bytes: Vec::from_slice_in(entry.bytes.as_slice(), self.alloc),
                modified_ns: entry.modified_ns,
                changed_ns: entry.changed_ns,
            });
        }
    }

    /// Returns whether `path` currently exists in the live filesystem view.
    pub fn exists(&self, path: &FileName) -> bool {
        self.find_live(path).is_some()
    }

    /// Returns the live bytes of `path`, if present.
    pub fn contents(&self, path: &FileName) -> Option<&[u8]> {
        let idx = self.find_live(path)?;
        Some(self.live[idx].live_bytes.as_slice())
    }

    fn next_tick(&mut self) -> u64 {
        let t = self.tick;
        self.tick += 1;
        t
    }

    fn next_ino(&mut self) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        ino
    }

    fn find_live(&self, path: &FileName) -> Option<usize> {
        self.live.as_slice().iter().position(|e| &e.path == path)
    }

    fn path_in_dir(path: &FileName, dir: &DirName) -> bool {
        match dir {
            DirName::Root => true,
            DirName::Named(prefix) => path.is_under(prefix),
        }
    }
}

impl<'a> DirectFsBackend for InMemoryDirectFs<'a> {
    fn open_write(&mut self, path: &FileName, create: bool, truncate: bool) -> Result<(), FsError> {
        if let Some(idx) = self.find_live(path) {
            if truncate {
                let tick = self.next_tick();
                self.live[idx].live_bytes.clear();
                self.live[idx].modified_ns = tick;
            }
            return Ok(());
        }
        if !create {
            return Err(FsError::NotFound { path: path.clone() });
        }
        let ino = self.next_ino();
        let tick = self.next_tick();
        self.live.push(MemFile {
            path: path.clone(),
            ino,
            live_bytes: Vec::new_in(self.alloc),
            flushed_bytes: Vec::new_in(self.alloc),
            modified_ns: tick,
            changed_ns: tick,
        });
        Ok(())
    }

    fn open_read(&mut self, path: &FileName) -> Result<(), FsError> {
        if self.find_live(path).is_some() {
            Ok(())
        } else {
            Err(FsError::NotFound { path: path.clone() })
        }
    }

    fn read_at(&mut self, path: &FileName, at: u64, dst: &mut [u8]) -> Result<usize, FsError> {
        let Some(idx) = self.find_live(path) else {
            return Err(FsError::NotFound { path: path.clone() });
        };
        let src = self.live[idx].live_bytes.as_slice();
        let start = usize::try_from(at).unwrap_or(usize::MAX);
        if start >= src.len() {
            return Ok(0);
        }
        let avail = src.len() - start;
        let n = avail.min(dst.len());
        dst[..n].copy_from_slice(&src[start..start + n]);
        Ok(n)
    }

    fn write_at(&mut self, path: &FileName, at: u64, src: &[u8]) -> Result<(), FsError> {
        let Some(idx) = self.find_live(path) else {
            return Err(FsError::NotFound { path: path.clone() });
        };
        let start = usize::try_from(at).map_err(|_| FsError::Io {
            path: Some(path.clone()),
            message: "write offset overflow".into(),
        })?;
        let end = start.checked_add(src.len()).ok_or_else(|| FsError::Io {
            path: Some(path.clone()),
            message: "write end overflow".into(),
        })?;
        let tick = self.next_tick();
        let entry = &mut self.live[idx];
        if end > entry.live_bytes.len() {
            entry.live_bytes.resize(end, 0);
        }
        entry.live_bytes.as_mut_slice()[start..end].copy_from_slice(src);
        entry.modified_ns = tick;
        Ok(())
    }

    fn flush(&mut self, path: &FileName) -> Result<(), FsError> {
        let Some(idx) = self.find_live(path) else {
            return Err(FsError::NotFound { path: path.clone() });
        };
        let ino = self.live[idx].ino;
        let modified_ns = self.live[idx].modified_ns;
        let snapshot = Vec::from_slice_in(self.live[idx].live_bytes.as_slice(), self.alloc);
        self.live[idx].flushed_bytes = snapshot;
        // If this inode's directory entry is already durable, flushing updates its on-disk bytes.
        for dur in self.durable.as_mut_slice() {
            if dur.ino == ino {
                dur.bytes = Vec::from_slice_in(self.live[idx].flushed_bytes.as_slice(), self.alloc);
                dur.modified_ns = modified_ns;
            }
        }
        Ok(())
    }

    fn rename(&mut self, from: &FileName, to: &FileName) -> Result<(), FsError> {
        let Some(from_idx) = self.find_live(from) else {
            return Err(FsError::NotFound { path: from.clone() });
        };
        if from == to {
            let tick = self.next_tick();
            self.live[from_idx].changed_ns = tick;
            return Ok(());
        }
        let tick = self.next_tick();
        if let Some(to_idx) = self.find_live(to) {
            let mut entry = self.live.remove_swap(from_idx);
            let actual_to = if to_idx == self.live.len() { from_idx } else { to_idx };
            entry.path = to.clone();
            entry.changed_ns = tick;
            self.live[actual_to] = entry;
        } else {
            self.live[from_idx].path = to.clone();
            self.live[from_idx].changed_ns = tick;
        }
        Ok(())
    }

    fn sync_dir(&mut self, dir: &DirName) -> Result<(), FsError> {
        let mut i = 0;
        while i < self.durable.len() {
            if Self::path_in_dir(&self.durable[i].path, dir) {
                let _ = self.durable.remove_swap(i);
            } else {
                i += 1;
            }
        }
        for entry in self.live.as_slice() {
            if Self::path_in_dir(&entry.path, dir) {
                self.durable.push(DurableFile {
                    path: entry.path.clone(),
                    ino: entry.ino,
                    bytes: Vec::from_slice_in(entry.flushed_bytes.as_slice(), self.alloc),
                    modified_ns: entry.modified_ns,
                    changed_ns: entry.changed_ns,
                });
            }
        }
        Ok(())
    }

    fn stat(&mut self, path: &FileName) -> Result<Option<Meta>, FsError> {
        let Some(idx) = self.find_live(path) else {
            return Ok(None);
        };
        let entry = &self.live[idx];
        Ok(Some(Meta {
            len: entry.live_bytes.len() as u64,
            dir: false,
            modified_ns: entry.modified_ns,
            changed_ns: entry.changed_ns,
            ino: entry.ino,
            dev: 1,
        }))
    }
}

/// Handler-level error for [`DirectFsHandler`] (handle leak on normal exit or table capacity hit).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirectFsHandlerError {
    LeakedHandles(LeakedHandles),
    TableFull(HandleError),
}

impl fmt::Display for DirectFsHandlerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DirectFsHandlerError::LeakedHandles(l) => write!(f, "{l}"),
            DirectFsHandlerError::TableFull(e) => write!(f, "{e}"),
        }
    }
}

struct DirectOpenFile {
    path: FileName,
    writable: bool,
}

/// Zero-`Effect`/`Done`-bridge `blaze_machine::Handler` driving typed filesystem requests directly
/// over a [`DirectFsBackend`] with branded generational handle tables and loud leak detection.
pub struct DirectFsHandler<'io, 'b, B: DirectFsBackend> {
    backend: &'b mut B,
    task: TaskId,
    files: HandleTable<'io, FileKind, DirectOpenFile>,
    dirs: HandleTable<'io, DirKind, DirName>,
    root_dir: Dir<'io, ReadWrite>,
}

impl<'io, 'b, B: DirectFsBackend> DirectFsHandler<'io, 'b, B> {
    /// Creates a `DirectFsHandler` bound to `backend`, branded with `brand`, and backed by `alloc`.
    pub fn new(
        backend: &'b mut B,
        task: TaskId,
        brand: Brand<'io>,
        alloc: Alloc<'io>,
    ) -> Self {
        let files = HandleTable::new(HandleTableOpts { brand, max_slots: 256, alloc });
        let mut dirs = HandleTable::new(HandleTableOpts { brand, max_slots: 64, alloc });
        let root_id = dirs.insert(task, DirName::Root).expect("root dir slot");
        let root_dir = Dir::from_id(root_id);
        Self { backend, task, files, dirs, root_dir }
    }

    #[inline]
    pub const fn root(&self) -> Dir<'io, ReadWrite> {
        self.root_dir
    }

    #[inline]
    pub fn backend(&self) -> &B {
        self.backend
    }

    #[inline]
    pub fn backend_mut(&mut self) -> &mut B {
        self.backend
    }

    /// Reclaims any open file handles for this task; fails loudly with
    /// [`DirectFsHandlerError::LeakedHandles`] if any handle remained open on [`ExitKind::Normal`].
    pub fn finish_task(&mut self, exit: ExitKind) -> Result<u32, DirectFsHandlerError> {
        let count = self
            .files
            .reclaim_task(self.task, ExitKind::Cancelled, |_| {})
            .expect("cancelled reclaim is infallible");
        match exit {
            ExitKind::Cancelled => Ok(count),
            ExitKind::Normal if count == 0 => Ok(0),
            ExitKind::Normal => Err(DirectFsHandlerError::LeakedHandles(LeakedHandles {
                task: self.task,
                count,
            })),
        }
    }

    fn resolve_path(base: &DirName, path: &FileName) -> FileName {
        match base {
            DirName::Root => path.clone(),
            DirName::Named(prefix) => {
                let joined = format!("{prefix}/{path}");
                FileName::new(&joined).unwrap_or_else(|| path.clone())
            }
        }
    }

    #[inline(never)]
    fn exec_open_write(
        &mut self,
        req: OpenWrite<'io>,
    ) -> Result<Result<File<'io, Unflushed>, FsError>, DirectFsHandlerError> {
        let OpenWrite { dir, path, create, truncate } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let full_path = Self::resolve_path(&base, &path);
        if let Err(e) = self.backend.open_write(&full_path, create, truncate) {
            return Ok(Err(e));
        }
        let id = self
            .files
            .insert(self.task, DirectOpenFile { path: full_path, writable: true })
            .map_err(DirectFsHandlerError::TableFull)?;
        Ok(Ok(File::from_id(id)))
    }

    #[inline(never)]
    fn exec_open_read(
        &mut self,
        req: OpenRead<'io>,
    ) -> Result<Result<File<'io, ReadOnly>, FsError>, DirectFsHandlerError> {
        let OpenRead { dir, path } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let full_path = Self::resolve_path(&base, &path);
        if let Err(e) = self.backend.open_read(&full_path) {
            return Ok(Err(e));
        }
        let id = self
            .files
            .insert(self.task, DirectOpenFile { path: full_path, writable: false })
            .map_err(DirectFsHandlerError::TableFull)?;
        Ok(Ok(File::from_id(id)))
    }

    #[inline(never)]
    fn exec_close(&mut self, req: Close<'io>) -> Result<Result<(), FsError>, DirectFsHandlerError> {
        let Close { file } = req;
        Ok(self.files.remove(file, self.task).map(|_| ()).map_err(FsError::StaleHandle))
    }

    #[inline(never)]
    fn exec_read_into<'a>(
        &mut self,
        req: ReadInto<'io, 'a>,
    ) -> Result<Result<FilledBuf<'a>, BufError<'a, FsError>>, DirectFsHandlerError> {
        let ReadInto { file, at, mut buf } = req;
        let path = match self.files.get(file) {
            Ok(entry) => entry.path.clone(),
            Err(e) => return Ok(Err(BufError { buf, error: FsError::StaleHandle(e) })),
        };
        let pre_sized = !buf.is_empty();
        let want = if pre_sized { buf.len() } else { buf.capacity() };
        if !pre_sized {
            buf.bytes.resize(want, 0);
        }
        match self.backend.read_at(&path, at.0, buf.as_mut_slice()) {
            Ok(n) => {
                if !pre_sized {
                    buf.bytes.truncate(n);
                }
                Ok(Ok(FilledBuf { buf, filled: ByteLen(n as u32) }))
            }
            Err(error) => {
                if !pre_sized {
                    buf.bytes.clear();
                }
                Ok(Err(BufError { buf, error }))
            }
        }
    }

    #[inline(never)]
    fn exec_write_from<'a>(
        &mut self,
        req: WriteFrom<'io, 'a>,
    ) -> Result<Result<DmaBuf<'a>, BufError<'a, FsError>>, DirectFsHandlerError> {
        let WriteFrom { file, at, buf } = req;
        let path = match self.files.get(file) {
            Ok(entry) if entry.writable => entry.path.clone(),
            Ok(entry) => {
                return Ok(Err(BufError {
                    buf,
                    error: FsError::Io {
                        path: Some(entry.path.clone()),
                        message: "file handle is read-only".into(),
                    },
                }));
            }
            Err(e) => return Ok(Err(BufError { buf, error: FsError::StaleHandle(e) })),
        };
        match self.backend.write_at(&path, at.0, buf.as_slice()) {
            Ok(()) => Ok(Ok(buf)),
            Err(error) => Ok(Err(BufError { buf, error })),
        }
    }

    #[inline(never)]
    fn exec_flush(
        &mut self,
        req: Flush<'io>,
    ) -> Result<
        Result<(File<'io, Flushed>, FlushedProof), (File<'io, Unflushed>, FsError)>,
        DirectFsHandlerError,
    > {
        let Flush { file } = req;
        let path = match self.files.get(file.id()) {
            Ok(entry) => entry.path.clone(),
            Err(e) => return Ok(Err((file, FsError::StaleHandle(e)))),
        };
        match self.backend.flush(&path) {
            Ok(()) => {
                let id = file.into_id();
                Ok(Ok((File::<Flushed>::from_id(id), FlushedProof { path })))
            }
            Err(e) => Ok(Err((file, e))),
        }
    }

    #[inline(never)]
    fn exec_rename_flushed(
        &mut self,
        req: RenameFlushed<'io>,
    ) -> Result<Result<(), FsError>, DirectFsHandlerError> {
        let RenameFlushed { dir, flushed, to } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let to_full = Self::resolve_path(&base, &to);
        Ok(self.backend.rename(&flushed.path, &to_full))
    }

    #[inline(never)]
    fn exec_sync_dir(
        &mut self,
        req: SyncDir<'io>,
    ) -> Result<Result<(), FsError>, DirectFsHandlerError> {
        let SyncDir { dir } = req;
        let dir_name = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        Ok(self.backend.sync_dir(&dir_name))
    }

    #[inline(never)]
    fn exec_stat(
        &mut self,
        req: Stat<'io>,
    ) -> Result<Result<Option<Meta>, FsError>, DirectFsHandlerError> {
        let Stat { dir, path } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let full_path = Self::resolve_path(&base, &path);
        Ok(self.backend.stat(&full_path))
    }

    #[inline(never)]
    fn exec_chain_write_flush<'a>(
        &mut self,
        req: Chain<WriteFrom<'io, 'a>, Flush<'io>>,
    ) -> Result<
        ChainOutcome<
            Result<DmaBuf<'a>, BufError<'a, FsError>>,
            Result<(File<'io, Flushed>, FlushedProof), (File<'io, Unflushed>, FsError)>,
        >,
        DirectFsHandlerError,
    > {
        let (first, second) = req.split();
        match self.exec_write_from(first)? {
            Ok(buf) => {
                let flushed = self.exec_flush(second)?;
                Ok(ChainOutcome::Both(Ok(buf), flushed))
            }
            Err(buf_err) => {
                // `IOSQE_IO_LINK` short-circuit: release the moved `File` slot without flushing.
                let Flush { file } = second;
                let _ = self.files.remove(file.into_id(), self.task);
                Ok(ChainOutcome::FirstOnly(Err(buf_err)))
            }
        }
    }

    #[inline(never)]
    fn exec_batch_stat<const CAP: usize>(
        &mut self,
        batch: Batch<Stat<'io>, CAP>,
    ) -> Result<FixedVec<BatchReady<Result<Option<Meta>, FsError>>, CAP>, DirectFsHandlerError> {
        let mut ready: FixedVec<BatchReady<Result<Option<Meta>, FsError>>, CAP> = FixedVec::new();
        for (idx, req) in batch.into_items().into_iter().enumerate() {
            let index = u16::try_from(idx).expect("batch slot index fits in u16");
            let answer = self.exec_stat(req)?;
            ready
                .push(BatchReady { index, answer })
                .ok()
                .expect("batch ready fits CAP");
        }
        Ok(ready)
    }
}

macro_rules! impl_direct_fs_handler {
    ([$($lt:lifetime),*], $req:ty, $ans:ty, $method:ident) => {
        impl<'io, $($lt,)* 'b, B: DirectFsBackend> Handler<$req> for DirectFsHandler<'io, 'b, B> {
            type Error = DirectFsHandlerError;

            fn perform<Out>(
                &mut self,
                request: $req,
                deliver: impl FnOnce(Perform<$ans>) -> Out,
            ) -> Result<Out, Self::Error> {
                let ans = self.$method(request)?;
                Ok(deliver(Perform::Answered(ans)))
            }
        }
    };
}

impl_direct_fs_handler!([], OpenWrite<'io>, Result<File<'io, Unflushed>, FsError>, exec_open_write);
impl_direct_fs_handler!([], OpenRead<'io>, Result<File<'io, ReadOnly>, FsError>, exec_open_read);
impl_direct_fs_handler!([], Close<'io>, Result<(), FsError>, exec_close);
impl_direct_fs_handler!(
    ['a],
    ReadInto<'io, 'a>,
    Result<FilledBuf<'a>, BufError<'a, FsError>>,
    exec_read_into
);
impl_direct_fs_handler!(
    ['a],
    WriteFrom<'io, 'a>,
    Result<DmaBuf<'a>, BufError<'a, FsError>>,
    exec_write_from
);
impl_direct_fs_handler!(
    [],
    Flush<'io>,
    Result<(File<'io, Flushed>, FlushedProof), (File<'io, Unflushed>, FsError)>,
    exec_flush
);
impl_direct_fs_handler!([], RenameFlushed<'io>, Result<(), FsError>, exec_rename_flushed);
impl_direct_fs_handler!([], SyncDir<'io>, Result<(), FsError>, exec_sync_dir);
impl_direct_fs_handler!([], Stat<'io>, Result<Option<Meta>, FsError>, exec_stat);
impl_direct_fs_handler!(
    ['a],
    Chain<WriteFrom<'io, 'a>, Flush<'io>>,
    ChainOutcome<
        Result<DmaBuf<'a>, BufError<'a, FsError>>,
        Result<(File<'io, Flushed>, FlushedProof), (File<'io, Unflushed>, FsError)>,
    >,
    exec_chain_write_flush
);

impl<'io, 'b, B: DirectFsBackend, const CAP: usize> Handler<Batch<Stat<'io>, CAP>>
    for DirectFsHandler<'io, 'b, B>
{
    type Error = DirectFsHandlerError;

    fn perform<Out>(
        &mut self,
        request: Batch<Stat<'io>, CAP>,
        deliver: impl FnOnce(
            Perform<FixedVec<BatchReady<Result<Option<Meta>, FsError>>, CAP>>,
        ) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = self.exec_batch_stat(request)?;
        Ok(deliver(Perform::Answered(ans)))
    }
}
