//! Generic [`DriverHandler<'io, 'd, D>`] implementing `blaze_machine::Handler` over ANY
//! [`crate::Driver`] (`Native`, `Threads`, `Kqueue`, `Uring`, or `Sim`; rule 30 & rule 78).
//!
//! Manages generational [`HandleTable`]s for files, directories, stream connections, listeners,
//! datagram sockets, and advisory locks, buffers out-of-order/asynchronous completions (such as
//! `Done::Accepted` arriving during `Connect`), and sweeps ALL handle tables on task exit with
//! loud leak detection on [`ExitKind::Normal`] and clean driver teardown on
//! [`ExitKind::Cancelled`].

use core::fmt;

use blaze_machine::{Handler, Perform};
use blaze_own::{Alloc, Brand, Vec};

use crate::handle::{
    Conn as ConnHandle, ConnKind, Dir, DirKind, ExitKind, File, FileKind, Flushed, HandleError,
    HandleTable, HandleTableOpts, LeakedHandles, Listener, ListenerKind, Lock as LockHandle,
    LockKind, ReadOnly, ReadWrite, SocketKind, TaskId, Unflushed,
};
use crate::request::net::{
    Accept, CloseConn, Connect, Listen, NetError, RecvInto, SendFrom, Unlisten,
};
use crate::request::{
    BufError, ByteLen, ByteOffset, Close, CopyData, DmaBuf, FillRandom, FilledBuf, Flush,
    FlushedProof, FsAnswer, FsError, FsRequest, IoAnswer, IoRequest, LENT_READ_MAX, List, Lock,
    LockOutcome, MakeDir, OpenRead, OpenWrite, Read, ReadAll, ReadInto, Remove, RemoveDir,
    RenameFlushed, RenameUnsynced, Sleep, Stat, SyncDir, Truncate, Unlock, WriteDurable,
    WriteFrom,
};
use crate::sim::Sim;
use crate::wait::Deadline;
use crate::{
    Address, Completion, Conn as RawConn, Dgram, DirName, Done, Driver, Effect, FileName, Listing,
    Meta, RANDOM_MOST, RECV_MOST, Tag,
};

/// Upper bound on poll iterations waiting for one completion tag (rule 18 `bounded-progress`).
const MAX_POLL_ROUNDS: u32 = 1 << 16;

/// Driver-level error from [`DriverHandler`] (e.g. a task exited normally with leaked handles,
/// a handle table hit its explicit bound, or a wait stalled with nothing in flight).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DriverHandlerError {
    LeakedHandles(LeakedHandles),
    TableFull(HandleError),
    Stalled { tag: Tag },
}

/// Alias preserving the Wave-1 `SimHandlerError` name for callers over `Sim`.
pub type SimHandlerError = DriverHandlerError;

impl fmt::Display for DriverHandlerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DriverHandlerError::LeakedHandles(l) => write!(f, "{l}"),
            DriverHandlerError::TableFull(e) => write!(f, "{e}"),
            DriverHandlerError::Stalled { tag } => {
                write!(f, "driver stalled waiting for completion of {tag:?}")
            }
        }
    }
}

struct OpenFileEntry {
    path: FileName,
    writable: bool,
}

struct OpenListenerEntry {
    tag: Tag,
    addr: Address,
}

struct OpenLockEntry {
    path: FileName,
    tag: Tag,
}

/// Generic `blaze_machine::Handler` driving typed [`FsRequest`] and [`IoRequest`] machines over
/// ANY [`crate::Driver`] (`Native`, `Threads`, `Kqueue`, `Uring`, `Sim`) with generational
/// branded handles, capped scratch buffers, explicit `NotFound`, completion demuxing, and loud
/// normal-exit leak detection across every handle table.
pub struct DriverHandler<'io, 'd, D: Driver> {
    driver: &'d mut D,
    task: TaskId,
    alloc: Alloc<'io>,
    files: HandleTable<'io, FileKind, OpenFileEntry>,
    dirs: HandleTable<'io, DirKind, DirName>,
    conns: HandleTable<'io, ConnKind, RawConn>,
    listeners: HandleTable<'io, ListenerKind, OpenListenerEntry>,
    sockets: HandleTable<'io, SocketKind, Dgram>,
    locks: HandleTable<'io, LockKind, OpenLockEntry>,
    root_dir: Dir<'io, ReadWrite>,
    scratch: Vec<u8, Alloc<'io>>,
    inbox: Vec<Completion, Alloc<'io>>,
    submit_buf: std::vec::Vec<Effect>,
    poll_buf: std::vec::Vec<Completion>,
    net_buf: std::vec::Vec<u8>,
    next_tag: u64,
}

/// Deterministic handler specialized over [`Sim`] (zero duplication, rule 30 & rule 78).
pub type SimHandler<'io, 'sim> = DriverHandler<'io, 'sim, Sim>;

impl<'io, 'd, D: Driver> DriverHandler<'io, 'd, D> {
    /// Creates a `DriverHandler` bound to `driver`, branded with `brand`, and backed by `alloc`.
    pub fn new(
        driver: &'d mut D,
        task: TaskId,
        brand: Brand<'io>,
        alloc: Alloc<'io>,
    ) -> Self {
        let files = HandleTable::new(HandleTableOpts { brand, max_slots: 256, alloc });
        let mut dirs = HandleTable::new(HandleTableOpts { brand, max_slots: 64, alloc });
        let conns = HandleTable::new(HandleTableOpts { brand, max_slots: 256, alloc });
        let listeners = HandleTable::new(HandleTableOpts { brand, max_slots: 64, alloc });
        let sockets = HandleTable::new(HandleTableOpts { brand, max_slots: 64, alloc });
        let locks = HandleTable::new(HandleTableOpts { brand, max_slots: 128, alloc });
        let root_id = dirs.insert(task, DirName::Root).expect("root dir slot");
        let root_dir = Dir::from_id(root_id);
        let scratch = Vec::with_capacity_in(LENT_READ_MAX.0 as usize, alloc);
        let inbox = Vec::new_in(alloc);
        Self {
            driver,
            task,
            alloc,
            files,
            dirs,
            conns,
            listeners,
            sockets,
            locks,
            root_dir,
            scratch,
            inbox,
            submit_buf: std::vec::Vec::new(),
            poll_buf: std::vec::Vec::new(),
            net_buf: std::vec::Vec::new(),
            next_tag: 1,
        }
    }

    #[inline]
    pub const fn root(&self) -> Dir<'io, ReadWrite> {
        self.root_dir
    }

    #[inline]
    pub fn driver(&self) -> &D {
        self.driver
    }

    #[inline]
    pub fn driver_mut(&mut self) -> &mut D {
        self.driver
    }

    /// Sweeps ALL handle tables (`files`, `conns`, `listeners`, `sockets`, `locks`) for this task.
    ///
    /// - Issues `Effect::Hangup` / `Effect::Unlisten` / `Effect::Unbind` / `Effect::Unlock` for
    ///   every reclaimed resource so the underlying driver never leaks OS/sim state.
    /// - On [`ExitKind::Normal`]: returns `Err(DriverHandlerError::LeakedHandles(..))` if ANY
    ///   handle across ANY table was left unclosed.
    /// - On [`ExitKind::Cancelled`]: returns `Ok(total_reclaimed)`.
    pub fn finish_task(&mut self, exit: ExitKind) -> Result<u32, DriverHandlerError> {
        let mut leaked_conns: Vec<RawConn, Alloc<'io>> = Vec::new_in(self.alloc);
        let mut leaked_listeners: Vec<OpenListenerEntry, Alloc<'io>> = Vec::new_in(self.alloc);
        let mut leaked_sockets: Vec<Dgram, Alloc<'io>> = Vec::new_in(self.alloc);
        let mut leaked_locks: Vec<OpenLockEntry, Alloc<'io>> = Vec::new_in(self.alloc);

        let file_count = self
            .files
            .reclaim_task(self.task, ExitKind::Cancelled, |_| {})
            .expect("cancelled reclaim is infallible");
        let conn_count = self
            .conns
            .reclaim_task(self.task, ExitKind::Cancelled, |c| leaked_conns.push(c))
            .expect("cancelled reclaim is infallible");
        let listener_count = self
            .listeners
            .reclaim_task(self.task, ExitKind::Cancelled, |l| leaked_listeners.push(l))
            .expect("cancelled reclaim is infallible");
        let socket_count = self
            .sockets
            .reclaim_task(self.task, ExitKind::Cancelled, |s| leaked_sockets.push(s))
            .expect("cancelled reclaim is infallible");
        let lock_count = self
            .locks
            .reclaim_task(self.task, ExitKind::Cancelled, |lk| leaked_locks.push(lk))
            .expect("cancelled reclaim is infallible");

        for conn in leaked_conns.as_slice() {
            let tag = self.alloc_tag();
            let _ = self.run_effect(Effect::Hangup { conn: *conn, tag });
        }
        for entry in leaked_listeners.as_slice() {
            let _ = self.unlisten_tag(entry.tag, Some(entry.addr.clone()));
        }
        for sock in leaked_sockets.as_slice() {
            let tag = self.alloc_tag();
            let _ = self.run_effect(Effect::Unbind { sock: *sock, tag });
        }
        for entry in leaked_locks.as_slice() {
            let tag = self.alloc_tag();
            let _ = self.run_effect(Effect::Unlock { lock: entry.tag, tag });
        }

        let total = file_count + conn_count + listener_count + socket_count + lock_count;
        match exit {
            ExitKind::Cancelled => Ok(total),
            ExitKind::Normal if total == 0 => Ok(0),
            ExitKind::Normal => Err(DriverHandlerError::LeakedHandles(LeakedHandles {
                task: self.task,
                count: total,
            })),
        }
    }

    fn alloc_tag(&mut self) -> Tag {
        let t = Tag(self.next_tag);
        self.next_tag += 1;
        t
    }

    fn submit_one(&mut self, effect: Effect) {
        self.submit_buf.clear();
        self.submit_buf.push(effect);
        self.driver.submit(&mut self.submit_buf);
    }

    fn drain_nonblocking(&mut self) {
        self.poll_buf.clear();
        self.driver.poll(&mut self.poll_buf, Deadline::PASSED);
        for comp in self.poll_buf.drain(..) {
            self.inbox.push(comp);
        }
    }

    fn take_buffered(&mut self, target: Tag) -> Option<Done> {
        let idx = self.inbox.as_slice().iter().position(|c| c.tag == target)?;
        Some(self.inbox.remove_shift(idx).done)
    }

    fn wait_for_tag(&mut self, target: Tag) -> Result<Done, DriverHandlerError> {
        if let Some(done) = self.take_buffered(target) {
            return Ok(done);
        }
        for _ in 0..MAX_POLL_ROUNDS {
            self.poll_buf.clear();
            let n = self.driver.poll(&mut self.poll_buf, Deadline::NEVER);
            let mut found = None;
            for comp in self.poll_buf.drain(..) {
                if found.is_none() && comp.tag == target {
                    found = Some(comp.done);
                } else {
                    self.inbox.push(comp);
                }
            }
            if let Some(done) = found {
                return Ok(done);
            }
            if n == 0 {
                return Err(DriverHandlerError::Stalled { tag: target });
            }
        }
        Err(DriverHandlerError::Stalled { tag: target })
    }

    fn run_effect(&mut self, effect: Effect) -> Result<Done, DriverHandlerError> {
        let tag = effect.tag();
        self.submit_one(effect);
        self.wait_for_tag(tag)
    }

    fn unlisten_tag(
        &mut self,
        listen_tag: Tag,
        addr: Option<Address>,
    ) -> Result<Result<(), NetError>, DriverHandlerError> {
        self.submit_one(Effect::Unlisten { tag: listen_tag });
        for _ in 0..MAX_POLL_ROUNDS {
            match self.wait_for_tag(listen_tag)? {
                Done::Unlistened => return Ok(Ok(())),
                Done::Accepted { conn } => {
                    // Peer connected right before unlisten: hang up cleanly so nothing leaks.
                    let hangup_tag = self.alloc_tag();
                    let _ = self.run_effect(Effect::Hangup { conn, tag: hangup_tag });
                }
                Done::Failed(e) => return Ok(Err(NetError::from_io(addr, e))),
                other => panic!("unexpected Unlisten completion: {other:?}"),
            }
        }
        Err(DriverHandlerError::Stalled { tag: listen_tag })
    }

    fn map_io_err(path: FileName, err: crate::IoError) -> FsError {
        let msg = err.to_string();
        if msg.contains("No space") || msg.contains("ENOSPC") || msg.contains("full") {
            FsError::NoSpace { path }
        } else if msg.contains("NotFound")
            || msg.contains("not found")
            || msg.contains("No such file")
            || msg.contains("no such file")
            || msg.contains("no such directory")
        {
            FsError::NotFound { path }
        } else {
            FsError::Io { path: Some(path), message: msg }
        }
    }

    fn map_dir_err(dir: DirName, err: crate::IoError) -> FsError {
        match dir {
            DirName::Named(path) => Self::map_io_err(path, err),
            DirName::Root => FsError::Io { path: None, message: err.to_string() },
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

    fn resolve_dir(base: &DirName, target: &DirName) -> DirName {
        match (base, target) {
            (DirName::Root, _) => target.clone(),
            (DirName::Named(_), DirName::Root) => base.clone(),
            (DirName::Named(_), DirName::Named(sub)) => {
                DirName::Named(Self::resolve_path(base, sub))
            }
        }
    }

    // -----------------------------------------------------------------------
    // Direct non-closure-generic execution methods (one per concrete request;
    // eliminates monomorphization blowup across caller closures, rule 16 & 30)
    // -----------------------------------------------------------------------

    #[inline(never)]
    fn exec_open_write(
        &mut self,
        req: OpenWrite<'io>,
    ) -> Result<Result<File<'io, Unflushed>, FsError>, DriverHandlerError> {
        let OpenWrite { dir, path, create: _, truncate } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let full_path = Self::resolve_path(&base, &path);
        if truncate {
            let tag = self.alloc_tag();
            match self.run_effect(Effect::Truncate { file: full_path.clone(), len: 0, tag })? {
                Done::Truncated => {}
                Done::Failed(e) => return Ok(Err(Self::map_io_err(full_path, e))),
                other => panic!("unexpected Truncate completion: {other:?}"),
            }
        }
        let id = self
            .files
            .insert(self.task, OpenFileEntry { path: full_path, writable: true })
            .map_err(DriverHandlerError::TableFull)?;
        Ok(Ok(File::from_id(id)))
    }

    #[inline(never)]
    fn exec_open_read(
        &mut self,
        req: OpenRead<'io>,
    ) -> Result<Result<File<'io, ReadOnly>, FsError>, DriverHandlerError> {
        let OpenRead { dir, path } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let full_path = Self::resolve_path(&base, &path);
        let tag = self.alloc_tag();
        match self.run_effect(Effect::Stat { file: full_path.clone(), tag })? {
            Done::Stat(Some(_)) => {}
            Done::Stat(None) => return Ok(Err(FsError::NotFound { path: full_path })),
            Done::Failed(e) => return Ok(Err(Self::map_io_err(full_path, e))),
            other => panic!("unexpected Stat completion: {other:?}"),
        }
        let id = self
            .files
            .insert(self.task, OpenFileEntry { path: full_path, writable: false })
            .map_err(DriverHandlerError::TableFull)?;
        Ok(Ok(File::from_id(id)))
    }

    #[inline(never)]
    fn exec_close(&mut self, req: Close<'io>) -> Result<Result<(), FsError>, DriverHandlerError> {
        let Close { file } = req;
        Ok(self.files.remove(file, self.task).map(|_| ()).map_err(FsError::StaleHandle))
    }

    #[inline(never)]
    fn exec_read<'s>(
        &'s mut self,
        req: Read<'io>,
    ) -> Result<Result<&'s [u8], FsError>, DriverHandlerError> {
        let Read { file, at, len } = req;
        if len.0 > LENT_READ_MAX.0 {
            return Ok(Err(FsError::LimitExceeded {
                requested: u64::from(len.0),
                max: u64::from(LENT_READ_MAX.0),
            }));
        }
        let path = match self.files.get(file) {
            Ok(entry) => entry.path.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let tag = self.alloc_tag();
        match self.run_effect(Effect::Read {
            file: path.clone(),
            at: at.0,
            len: len.0 as usize,
            tag,
        })? {
            Done::Read(bytes) => {
                self.scratch.clear();
                self.scratch.extend_from_slice(&bytes);
                Ok(Ok(self.scratch.as_slice()))
            }
            Done::Failed(e) => Ok(Err(Self::map_io_err(path, e))),
            other => panic!("unexpected Read completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_read_into<'a>(
        &mut self,
        req: ReadInto<'io, 'a>,
    ) -> Result<Result<FilledBuf<'a>, BufError<'a, FsError>>, DriverHandlerError> {
        let ReadInto { file, at, mut buf } = req;
        let path = match self.files.get(file) {
            Ok(entry) => entry.path.clone(),
            Err(e) => return Ok(Err(BufError { buf, error: FsError::StaleHandle(e) })),
        };
        let pre_sized = !buf.is_empty();
        let want = if pre_sized { buf.len() } else { buf.capacity() };
        let tag = self.alloc_tag();
        match self.run_effect(Effect::Read { file: path.clone(), at: at.0, len: want, tag })? {
            Done::Read(bytes) => {
                let n = bytes.len().min(want);
                if pre_sized {
                    buf.as_mut_slice()[..n].copy_from_slice(&bytes[..n]);
                } else {
                    buf.bytes.extend_from_slice(&bytes[..n]);
                }
                Ok(Ok(FilledBuf { buf, filled: ByteLen(n as u32) }))
            }
            Done::Failed(e) => Ok(Err(BufError { buf, error: Self::map_io_err(path, e) })),
            other => panic!("unexpected ReadInto completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_write_from<'a>(
        &mut self,
        req: WriteFrom<'io, 'a>,
    ) -> Result<Result<DmaBuf<'a>, BufError<'a, FsError>>, DriverHandlerError> {
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
        let tag = self.alloc_tag();
        let mut payload = std::vec::Vec::with_capacity(buf.len());
        payload.extend_from_slice(buf.as_slice());
        // Use Append for offset-0 writes so `Sim::crash` exercises unsynced tearing.
        let effect = if at.0 == 0 {
            Effect::Append { file: path.clone(), bytes: payload, tag }
        } else {
            Effect::WriteAt { file: path.clone(), at: at.0, bytes: payload, tag }
        };
        match self.run_effect(effect)? {
            Done::Appended | Done::Written => Ok(Ok(buf)),
            Done::Failed(e) => Ok(Err(BufError { buf, error: Self::map_io_err(path, e) })),
            other => panic!("unexpected WriteFrom completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_flush(
        &mut self,
        req: Flush<'io>,
    ) -> Result<
        Result<(File<'io, Flushed>, FlushedProof), (File<'io, Unflushed>, FsError)>,
        DriverHandlerError,
    > {
        let Flush { file } = req;
        let path = match self.files.get(file.id()) {
            Ok(entry) => entry.path.clone(),
            Err(e) => return Ok(Err((file, FsError::StaleHandle(e)))),
        };
        let tag = self.alloc_tag();
        match self.run_effect(Effect::Sync { file: path.clone(), tag })? {
            Done::Synced => {
                let id = file.into_id();
                let flushed = File::<Flushed>::from_id(id);
                let proof = FlushedProof { path };
                Ok(Ok((flushed, proof)))
            }
            Done::Failed(e) => Ok(Err((file, Self::map_io_err(path, e)))),
            other => panic!("unexpected Sync completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_read_all<'a>(
        &mut self,
        req: ReadAll<'io, 'a>,
    ) -> Result<Result<FilledBuf<'a>, BufError<'a, FsError>>, DriverHandlerError> {
        let ReadAll { dir, path, max, mut buf } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(BufError { buf, error: FsError::StaleHandle(e) })),
        };
        let full_path = Self::resolve_path(&base, &path);
        // Explicitly check existence so absent is NEVER misread as empty!
        let stat_tag = self.alloc_tag();
        match self.run_effect(Effect::Stat { file: full_path.clone(), tag: stat_tag })? {
            Done::Stat(None) => {
                return Ok(Err(BufError { buf, error: FsError::NotFound { path: full_path } }));
            }
            Done::Stat(Some(meta)) if meta.len > max.0 => {
                return Ok(Err(BufError {
                    buf,
                    error: FsError::LimitExceeded { requested: meta.len, max: max.0 },
                }));
            }
            Done::Stat(Some(_)) => {}
            Done::Failed(e) => {
                return Ok(Err(BufError { buf, error: Self::map_io_err(full_path, e) }));
            }
            other => panic!("unexpected Stat completion: {other:?}"),
        }
        let tag = self.alloc_tag();
        match self.run_effect(Effect::ReadAll { file: full_path.clone(), tag })? {
            Done::Read(bytes) => {
                if (bytes.len() as u64) > max.0 {
                    return Ok(Err(BufError {
                        buf,
                        error: FsError::LimitExceeded {
                            requested: bytes.len() as u64,
                            max: max.0,
                        },
                    }));
                }
                buf.clear();
                buf.bytes.extend_from_slice(&bytes);
                let filled = ByteLen(bytes.len() as u32);
                Ok(Ok(FilledBuf { buf, filled }))
            }
            Done::Failed(e) => Ok(Err(BufError { buf, error: Self::map_io_err(full_path, e) })),
            other => panic!("unexpected ReadAll completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_rename_flushed(
        &mut self,
        req: RenameFlushed<'io>,
    ) -> Result<Result<(), FsError>, DriverHandlerError> {
        let RenameFlushed { dir, flushed, to } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let to_full = Self::resolve_path(&base, &to);
        let tag = self.alloc_tag();
        match self.run_effect(Effect::Rename { from: flushed.path, to: to_full.clone(), tag })? {
            Done::Renamed => Ok(Ok(())),
            Done::Failed(e) => Ok(Err(Self::map_io_err(to_full, e))),
            other => panic!("unexpected Rename completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_rename_unsynced(
        &mut self,
        req: RenameUnsynced<'io>,
    ) -> Result<Result<(), FsError>, DriverHandlerError> {
        let RenameUnsynced { dir, from, to } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let from_full = Self::resolve_path(&base, &from);
        let to_full = Self::resolve_path(&base, &to);
        let tag = self.alloc_tag();
        match self.run_effect(Effect::RenameUnsynced {
            from: from_full,
            to: to_full.clone(),
            tag,
        })? {
            Done::Renamed => Ok(Ok(())),
            Done::Failed(e) => Ok(Err(Self::map_io_err(to_full, e))),
            other => panic!("unexpected RenameUnsynced completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_sync_dir(
        &mut self,
        req: SyncDir<'io>,
    ) -> Result<Result<(), FsError>, DriverHandlerError> {
        let SyncDir { dir } = req;
        let dir_name = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let tag = self.alloc_tag();
        match self.run_effect(Effect::SyncDir { dir: dir_name.clone(), tag })? {
            Done::Synced => Ok(Ok(())),
            Done::Failed(e) => Ok(Err(Self::map_dir_err(dir_name, e))),
            other => panic!("unexpected SyncDir completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_stat(
        &mut self,
        req: Stat<'io>,
    ) -> Result<Result<Option<Meta>, FsError>, DriverHandlerError> {
        let Stat { dir, path } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let full_path = Self::resolve_path(&base, &path);
        let tag = self.alloc_tag();
        match self.run_effect(Effect::Stat { file: full_path.clone(), tag })? {
            Done::Stat(meta) => Ok(Ok(meta)),
            Done::Failed(e) => Ok(Err(Self::map_io_err(full_path, e))),
            other => panic!("unexpected Stat completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_list(&mut self, req: List<'io>) -> Result<Result<Listing, FsError>, DriverHandlerError> {
        let List { scope, dir } = req;
        let base = match self.dirs.get(scope.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let target = Self::resolve_dir(&base, &dir);
        let tag = self.alloc_tag();
        match self.run_effect(Effect::Entries { dir: target.clone(), tag })? {
            Done::Entries(listing) => Ok(Ok(*listing)),
            Done::Failed(e) => Ok(Err(Self::map_dir_err(target, e))),
            other => panic!("unexpected Entries completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_remove(&mut self, req: Remove<'io>) -> Result<Result<(), FsError>, DriverHandlerError> {
        let Remove { dir, path } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let full_path = Self::resolve_path(&base, &path);
        let tag = self.alloc_tag();
        match self.run_effect(Effect::Remove { file: full_path.clone(), tag })? {
            Done::Removed => Ok(Ok(())),
            Done::Failed(e) => Ok(Err(Self::map_io_err(full_path, e))),
            other => panic!("unexpected Remove completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_make_dir(
        &mut self,
        req: MakeDir<'io>,
    ) -> Result<Result<(), FsError>, DriverHandlerError> {
        let MakeDir { dir, path } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let full_path = Self::resolve_path(&base, &path);
        let tag = self.alloc_tag();
        match self.run_effect(Effect::MakeDir { dir: full_path.clone(), tag })? {
            Done::MadeDir => Ok(Ok(())),
            Done::Failed(e) => Ok(Err(Self::map_io_err(full_path, e))),
            other => panic!("unexpected MakeDir completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_remove_dir(
        &mut self,
        req: RemoveDir<'io>,
    ) -> Result<Result<(), FsError>, DriverHandlerError> {
        let RemoveDir { dir, path } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let full_path = Self::resolve_path(&base, &path);
        let tag = self.alloc_tag();
        match self.run_effect(Effect::RemoveDir { dir: full_path.clone(), tag })? {
            Done::RemovedDir => Ok(Ok(())),
            Done::Failed(e) => Ok(Err(Self::map_io_err(full_path, e))),
            other => panic!("unexpected RemoveDir completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_truncate(
        &mut self,
        req: Truncate<'io>,
    ) -> Result<Result<(), FsError>, DriverHandlerError> {
        let Truncate { file, len } = req;
        let path = match self.files.get(file) {
            Ok(entry) if entry.writable => entry.path.clone(),
            Ok(entry) => {
                return Ok(Err(FsError::Io {
                    path: Some(entry.path.clone()),
                    message: "file handle is read-only".into(),
                }));
            }
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let tag = self.alloc_tag();
        match self.run_effect(Effect::Truncate { file: path.clone(), len: len.0, tag })? {
            Done::Truncated => Ok(Ok(())),
            Done::Failed(e) => Ok(Err(Self::map_io_err(path, e))),
            other => panic!("unexpected Truncate completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_lock(
        &mut self,
        req: Lock<'io>,
    ) -> Result<Result<LockOutcome<'io>, FsError>, DriverHandlerError> {
        let Lock { dir, path, mode } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let full_path = Self::resolve_path(&base, &path);
        let lock_tag = self.alloc_tag();
        match self.run_effect(Effect::Lock { file: full_path.clone(), mode, tag: lock_tag })? {
            Done::Locked => match self
                .locks
                .insert(self.task, OpenLockEntry { path: full_path, tag: lock_tag })
            {
                Ok(id) => Ok(Ok(LockOutcome::Acquired(LockHandle::from_id(id)))),
                Err(e) => {
                    // Release the acquired lock immediately so the driver never leaks.
                    let unlock_tag = self.alloc_tag();
                    let _ = self.run_effect(Effect::Unlock { lock: lock_tag, tag: unlock_tag });
                    Err(DriverHandlerError::TableFull(e))
                }
            },
            Done::Contended => Ok(Ok(LockOutcome::Contended)),
            Done::Failed(e) => Ok(Err(Self::map_io_err(full_path, e))),
            other => panic!("unexpected Lock completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_unlock(&mut self, req: Unlock<'io>) -> Result<Result<(), FsError>, DriverHandlerError> {
        let Unlock { lock } = req;
        let entry = match self.locks.remove(lock, self.task) {
            Ok(e) => e,
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let tag = self.alloc_tag();
        match self.run_effect(Effect::Unlock { lock: entry.tag, tag })? {
            Done::Unlocked => Ok(Ok(())),
            Done::Failed(e) => Ok(Err(Self::map_io_err(entry.path, e))),
            other => panic!("unexpected Unlock completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_copy_data(
        &mut self,
        req: CopyData<'io>,
    ) -> Result<Result<ByteOffset, FsError>, DriverHandlerError> {
        let CopyData { dir, from, to } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(FsError::StaleHandle(e))),
        };
        let from_full = Self::resolve_path(&base, &from);
        let to_full = Self::resolve_path(&base, &to);
        let tag = self.alloc_tag();
        match self.run_effect(Effect::CopyData { from: from_full, to: to_full.clone(), tag })? {
            Done::Copied { bytes } => Ok(Ok(ByteOffset(bytes))),
            Done::Failed(e) => Ok(Err(Self::map_io_err(to_full, e))),
            other => panic!("unexpected CopyData completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_write_durable<'a>(
        &mut self,
        req: WriteDurable<'io, 'a>,
    ) -> Result<Result<DmaBuf<'a>, BufError<'a, FsError>>, DriverHandlerError> {
        let WriteDurable { dir, path, buf } = req;
        let base = match self.dirs.get(dir.id()) {
            Ok(d) => d.clone(),
            Err(e) => return Ok(Err(BufError { buf, error: FsError::StaleHandle(e) })),
        };
        let full_path = Self::resolve_path(&base, &path);
        let tag = self.alloc_tag();
        let mut payload = std::vec::Vec::with_capacity(buf.len());
        payload.extend_from_slice(buf.as_slice());
        match self.run_effect(Effect::Write { file: full_path.clone(), bytes: payload, tag })? {
            Done::Written => Ok(Ok(buf)),
            Done::Failed(e) => Ok(Err(BufError { buf, error: Self::map_io_err(full_path, e) })),
            other => panic!("unexpected Write completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_listen(
        &mut self,
        req: Listen<'io>,
    ) -> Result<Result<Listener<'io>, NetError>, DriverHandlerError> {
        let Listen { addr, _brand: _ } = req;
        let listen_tag = self.alloc_tag();
        self.submit_one(Effect::Listen {
            at: addr.clone(),
            options: crate::sockopt::Tcp::DEFAULT,
            tag: listen_tag,
        });
        // Check for an immediate bind error without blocking (a healthy listen emits
        // no completion until a peer connects).
        self.drain_nonblocking();
        if let Some(done) = self.take_buffered(listen_tag) {
            match done {
                Done::Failed(e) => return Ok(Err(NetError::from_io(Some(addr), e))),
                other => {
                    // e.g. an immediate Accepted: re-queue for the caller's Accept!
                    self.inbox.push(Completion { tag: listen_tag, done: other });
                }
            }
        }
        let id = match self
            .listeners
            .insert(self.task, OpenListenerEntry { tag: listen_tag, addr: addr.clone() })
        {
            Ok(id) => id,
            Err(e) => {
                let _ = self.unlisten_tag(listen_tag, Some(addr));
                return Err(DriverHandlerError::TableFull(e));
            }
        };
        Ok(Ok(Listener::from_id(id)))
    }

    #[inline(never)]
    fn exec_accept(
        &mut self,
        req: Accept<'io>,
    ) -> Result<Result<ConnHandle<'io>, NetError>, DriverHandlerError> {
        let Accept { listener } = req;
        let (listen_tag, addr) = match self.listeners.get(listener) {
            Ok(entry) => (entry.tag, entry.addr.clone()),
            Err(e) => return Ok(Err(NetError::StaleHandle(e))),
        };
        match self.wait_for_tag(listen_tag)? {
            Done::Accepted { conn } => {
                let id = match self.conns.insert(self.task, conn) {
                    Ok(id) => id,
                    Err(e) => {
                        let hangup_tag = self.alloc_tag();
                        let _ = self.run_effect(Effect::Hangup { conn, tag: hangup_tag });
                        return Err(DriverHandlerError::TableFull(e));
                    }
                };
                Ok(Ok(ConnHandle::from_id(id)))
            }
            Done::Unlistened => Ok(Err(NetError::Closed)),
            Done::Failed(e) => Ok(Err(NetError::from_io(Some(addr), e))),
            other => panic!("unexpected Accept completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_connect(
        &mut self,
        req: Connect<'io>,
    ) -> Result<Result<ConnHandle<'io>, NetError>, DriverHandlerError> {
        let Connect { addr, _brand: _ } = req;
        let tag = self.alloc_tag();
        match self.run_effect(Effect::Connect {
            at: addr.clone(),
            options: crate::sockopt::Tcp::DEFAULT,
            tag,
        })? {
            Done::Connected { conn } => {
                let id = match self.conns.insert(self.task, conn) {
                    Ok(id) => id,
                    Err(e) => {
                        let hangup_tag = self.alloc_tag();
                        let _ = self.run_effect(Effect::Hangup { conn, tag: hangup_tag });
                        return Err(DriverHandlerError::TableFull(e));
                    }
                };
                Ok(Ok(ConnHandle::from_id(id)))
            }
            Done::Failed(e) => Ok(Err(NetError::from_io(Some(addr), e))),
            other => panic!("unexpected Connect completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_send_from<'a>(
        &mut self,
        req: SendFrom<'io, 'a>,
    ) -> Result<Result<DmaBuf<'a>, BufError<'a, NetError>>, DriverHandlerError> {
        let SendFrom { conn, buf } = req;
        let raw_conn = match self.conns.get(conn) {
            Ok(c) => *c,
            Err(e) => return Ok(Err(BufError { buf, error: NetError::StaleHandle(e) })),
        };
        let tag = self.alloc_tag();
        let mut payload = core::mem::take(&mut self.net_buf);
        payload.clear();
        payload.extend_from_slice(buf.as_slice());
        match self.run_effect(Effect::Send { conn: raw_conn, bytes: payload, tag })? {
            Done::Sent(returned) => {
                self.net_buf = returned;
                Ok(Ok(buf))
            }
            Done::Broken { buffer, error } => {
                self.net_buf = buffer;
                Ok(Err(BufError { buf, error: NetError::from_io(None, error) }))
            }
            Done::Failed(e) => Ok(Err(BufError { buf, error: NetError::from_io(None, e) })),
            other => panic!("unexpected Send completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_recv_into<'a>(
        &mut self,
        req: RecvInto<'io, 'a>,
    ) -> Result<Result<FilledBuf<'a>, BufError<'a, NetError>>, DriverHandlerError> {
        let RecvInto { conn, mut buf } = req;
        let raw_conn = match self.conns.get(conn) {
            Ok(c) => *c,
            Err(e) => return Ok(Err(BufError { buf, error: NetError::StaleHandle(e) })),
        };
        let pre_sized = !buf.is_empty();
        let want = if pre_sized { buf.len() } else { buf.capacity() };
        let most = want.min(RECV_MOST);
        let tag = self.alloc_tag();
        let mut into = core::mem::take(&mut self.net_buf);
        into.clear();
        match self.run_effect(Effect::Recv { conn: raw_conn, most, into, tag })? {
            Done::Read(bytes) => {
                let n = bytes.len().min(want);
                if pre_sized {
                    buf.as_mut_slice()[..n].copy_from_slice(&bytes[..n]);
                } else {
                    buf.bytes.extend_from_slice(&bytes[..n]);
                }
                self.net_buf = bytes;
                Ok(Ok(FilledBuf { buf, filled: ByteLen(n as u32) }))
            }
            Done::Broken { buffer, error } => {
                self.net_buf = buffer;
                Ok(Err(BufError { buf, error: NetError::from_io(None, error) }))
            }
            Done::Failed(e) => Ok(Err(BufError { buf, error: NetError::from_io(None, e) })),
            other => panic!("unexpected Recv completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_close_conn(
        &mut self,
        req: CloseConn<'io>,
    ) -> Result<Result<(), NetError>, DriverHandlerError> {
        let CloseConn { conn } = req;
        let raw_conn = match self.conns.remove(conn, self.task) {
            Ok(c) => c,
            Err(e) => return Ok(Err(NetError::StaleHandle(e))),
        };
        let tag = self.alloc_tag();
        match self.run_effect(Effect::Hangup { conn: raw_conn, tag })? {
            Done::HungUp => Ok(Ok(())),
            Done::Failed(e) => Ok(Err(NetError::from_io(None, e))),
            other => panic!("unexpected Hangup completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_unlisten(
        &mut self,
        req: Unlisten<'io>,
    ) -> Result<Result<(), NetError>, DriverHandlerError> {
        let Unlisten { listener } = req;
        let entry = match self.listeners.remove(listener, self.task) {
            Ok(e) => e,
            Err(e) => return Ok(Err(NetError::StaleHandle(e))),
        };
        self.unlisten_tag(entry.tag, Some(entry.addr))
    }

    #[inline(never)]
    fn exec_sleep(&mut self, req: Sleep) -> Result<(), DriverHandlerError> {
        let Sleep { after } = req;
        let tag = self.alloc_tag();
        match self.run_effect(Effect::Timer { after, tag })? {
            Done::Fired => Ok(()),
            other => panic!("unexpected Timer completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_fill_random<'a>(
        &mut self,
        req: FillRandom<'a>,
    ) -> Result<Result<DmaBuf<'a>, BufError<'a, FsError>>, DriverHandlerError> {
        let FillRandom { mut buf } = req;
        let pre_sized = !buf.is_empty();
        let want = if pre_sized { buf.len() } else { buf.capacity() };
        if want > RANDOM_MOST {
            return Ok(Err(BufError {
                buf,
                error: FsError::LimitExceeded {
                    requested: want as u64,
                    max: RANDOM_MOST as u64,
                },
            }));
        }
        let tag = self.alloc_tag();
        match self.run_effect(Effect::Random { len: want, tag })? {
            Done::Random(bytes) => {
                let n = bytes.len().min(want);
                if pre_sized {
                    buf.as_mut_slice()[..n].copy_from_slice(&bytes[..n]);
                } else {
                    buf.bytes.extend_from_slice(&bytes[..n]);
                }
                Ok(Ok(buf))
            }
            Done::Failed(e) => Ok(Err(BufError {
                buf,
                error: FsError::Io { path: None, message: e.to_string() },
            })),
            other => panic!("unexpected Random completion: {other:?}"),
        }
    }

    #[inline(never)]
    fn exec_fs<'s, 'a>(
        &'s mut self,
        request: FsRequest<'io, 'a>,
    ) -> Result<FsAnswer<'io, 'a, 's>, DriverHandlerError> {
        let ans = match request {
            FsRequest::OpenWrite(r) => FsAnswer::OpenedWrite(self.exec_open_write(r)?),
            FsRequest::OpenRead(r) => FsAnswer::OpenedRead(self.exec_open_read(r)?),
            FsRequest::Close(r) => FsAnswer::Closed(self.exec_close(r)?),
            FsRequest::Read(r) => FsAnswer::Read(self.exec_read(r)?),
            FsRequest::ReadInto(r) => FsAnswer::ReadInto(self.exec_read_into(r)?),
            FsRequest::WriteFrom(r) => FsAnswer::Written(self.exec_write_from(r)?),
            FsRequest::Flush(r) => FsAnswer::Flushed(self.exec_flush(r)?),
            FsRequest::ReadAll(r) => FsAnswer::ReadAll(self.exec_read_all(r)?),
            FsRequest::RenameFlushed(r) => FsAnswer::Renamed(self.exec_rename_flushed(r)?),
            FsRequest::RenameUnsynced(r) => FsAnswer::Renamed(self.exec_rename_unsynced(r)?),
            FsRequest::SyncDir(r) => FsAnswer::SyncedDir(self.exec_sync_dir(r)?),
            FsRequest::Stat(r) => FsAnswer::Stat(self.exec_stat(r)?),
            FsRequest::List(r) => FsAnswer::Listed(self.exec_list(r)?),
            FsRequest::Remove(r) => FsAnswer::Removed(self.exec_remove(r)?),
            FsRequest::MakeDir(r) => FsAnswer::MadeDir(self.exec_make_dir(r)?),
            FsRequest::RemoveDir(r) => FsAnswer::RemovedDir(self.exec_remove_dir(r)?),
            FsRequest::Truncate(r) => FsAnswer::Truncated(self.exec_truncate(r)?),
            FsRequest::Lock(r) => FsAnswer::Locked(self.exec_lock(r)?),
            FsRequest::Unlock(r) => FsAnswer::Unlocked(self.exec_unlock(r)?),
            FsRequest::CopyData(r) => FsAnswer::CopiedData(self.exec_copy_data(r)?),
            FsRequest::WriteDurable(r) => FsAnswer::WrittenDurable(self.exec_write_durable(r)?),
        };
        Ok(ans)
    }

    #[inline(never)]
    fn exec_io<'s, 'a>(
        &'s mut self,
        request: IoRequest<'io, 'a>,
    ) -> Result<IoAnswer<'io, 'a, 's>, DriverHandlerError> {
        let ans = match request {
            IoRequest::OpenWrite(r) => IoAnswer::OpenedWrite(self.exec_open_write(r)?),
            IoRequest::OpenRead(r) => IoAnswer::OpenedRead(self.exec_open_read(r)?),
            IoRequest::Close(r) => IoAnswer::Closed(self.exec_close(r)?),
            IoRequest::Read(r) => IoAnswer::Read(self.exec_read(r)?),
            IoRequest::ReadInto(r) => IoAnswer::ReadInto(self.exec_read_into(r)?),
            IoRequest::WriteFrom(r) => IoAnswer::Written(self.exec_write_from(r)?),
            IoRequest::Flush(r) => IoAnswer::Flushed(self.exec_flush(r)?),
            IoRequest::ReadAll(r) => IoAnswer::ReadAll(self.exec_read_all(r)?),
            IoRequest::RenameFlushed(r) => IoAnswer::Renamed(self.exec_rename_flushed(r)?),
            IoRequest::RenameUnsynced(r) => IoAnswer::Renamed(self.exec_rename_unsynced(r)?),
            IoRequest::SyncDir(r) => IoAnswer::SyncedDir(self.exec_sync_dir(r)?),
            IoRequest::Stat(r) => IoAnswer::Stat(self.exec_stat(r)?),
            IoRequest::List(r) => IoAnswer::Listed(self.exec_list(r)?),
            IoRequest::Remove(r) => IoAnswer::Removed(self.exec_remove(r)?),
            IoRequest::MakeDir(r) => IoAnswer::MadeDir(self.exec_make_dir(r)?),
            IoRequest::RemoveDir(r) => IoAnswer::RemovedDir(self.exec_remove_dir(r)?),
            IoRequest::Truncate(r) => IoAnswer::Truncated(self.exec_truncate(r)?),
            IoRequest::Lock(r) => IoAnswer::Locked(self.exec_lock(r)?),
            IoRequest::Unlock(r) => IoAnswer::Unlocked(self.exec_unlock(r)?),
            IoRequest::CopyData(r) => IoAnswer::CopiedData(self.exec_copy_data(r)?),
            IoRequest::WriteDurable(r) => IoAnswer::WrittenDurable(self.exec_write_durable(r)?),
            IoRequest::Listen(r) => IoAnswer::Listened(self.exec_listen(r)?),
            IoRequest::Accept(r) => IoAnswer::Accepted(self.exec_accept(r)?),
            IoRequest::Connect(r) => IoAnswer::Connected(self.exec_connect(r)?),
            IoRequest::SendFrom(r) => IoAnswer::Sent(self.exec_send_from(r)?),
            IoRequest::RecvInto(r) => IoAnswer::Recved(self.exec_recv_into(r)?),
            IoRequest::CloseConn(r) => IoAnswer::ClosedConn(self.exec_close_conn(r)?),
            IoRequest::Unlisten(r) => IoAnswer::Unlistened(self.exec_unlisten(r)?),
            IoRequest::Sleep(r) => {
                self.exec_sleep(r)?;
                IoAnswer::Slept
            }
            IoRequest::FillRandom(r) => IoAnswer::FilledRandom(self.exec_fill_random(r)?),
        };
        Ok(ans)
    }
}

impl<'io, 'sim> DriverHandler<'io, 'sim, Sim> {
    #[inline]
    pub fn sim_mut(&mut self) -> &mut Sim {
        self.driver
    }
}

macro_rules! impl_direct_handler {
    ([$($lt:lifetime),*], $req:ty, $ans:ty, $method:ident) => {
        impl<'io, $($lt,)* 'd, D: Driver> Handler<$req> for DriverHandler<'io, 'd, D> {
            type Error = DriverHandlerError;

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

// Single-operation concrete handlers (no `FsRequest`/`IoRequest` boxing/unboxing).
impl_direct_handler!([], OpenWrite<'io>, Result<File<'io, Unflushed>, FsError>, exec_open_write);
impl_direct_handler!([], OpenRead<'io>, Result<File<'io, ReadOnly>, FsError>, exec_open_read);
impl_direct_handler!([], Close<'io>, Result<(), FsError>, exec_close);
impl<'io, 'd, D: Driver> Handler<Read<'io>> for DriverHandler<'io, 'd, D> {
    type Error = DriverHandlerError;

    fn perform<Out>(
        &mut self,
        request: Read<'io>,
        deliver: impl FnOnce(Perform<Result<&'_ [u8], FsError>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = self.exec_read(request)?;
        Ok(deliver(Perform::Answered(ans)))
    }
}
impl_direct_handler!(['a], ReadInto<'io, 'a>, Result<FilledBuf<'a>, BufError<'a, FsError>>, exec_read_into);
impl_direct_handler!(['a], WriteFrom<'io, 'a>, Result<DmaBuf<'a>, BufError<'a, FsError>>, exec_write_from);
impl_direct_handler!(
    [],
    Flush<'io>,
    Result<(File<'io, Flushed>, FlushedProof), (File<'io, Unflushed>, FsError)>,
    exec_flush
);
impl_direct_handler!(['a], ReadAll<'io, 'a>, Result<FilledBuf<'a>, BufError<'a, FsError>>, exec_read_all);
impl_direct_handler!([], RenameFlushed<'io>, Result<(), FsError>, exec_rename_flushed);
impl_direct_handler!([], RenameUnsynced<'io>, Result<(), FsError>, exec_rename_unsynced);
impl_direct_handler!([], SyncDir<'io>, Result<(), FsError>, exec_sync_dir);
impl_direct_handler!([], Stat<'io>, Result<Option<Meta>, FsError>, exec_stat);
impl_direct_handler!([], List<'io>, Result<Listing, FsError>, exec_list);
impl_direct_handler!([], Remove<'io>, Result<(), FsError>, exec_remove);
impl_direct_handler!([], MakeDir<'io>, Result<(), FsError>, exec_make_dir);
impl_direct_handler!([], RemoveDir<'io>, Result<(), FsError>, exec_remove_dir);
impl_direct_handler!([], Truncate<'io>, Result<(), FsError>, exec_truncate);
impl_direct_handler!([], Lock<'io>, Result<LockOutcome<'io>, FsError>, exec_lock);
impl_direct_handler!([], Unlock<'io>, Result<(), FsError>, exec_unlock);
impl_direct_handler!([], CopyData<'io>, Result<ByteOffset, FsError>, exec_copy_data);
impl_direct_handler!(['a], WriteDurable<'io, 'a>, Result<DmaBuf<'a>, BufError<'a, FsError>>, exec_write_durable);
impl_direct_handler!([], Listen<'io>, Result<Listener<'io>, NetError>, exec_listen);
impl_direct_handler!([], Accept<'io>, Result<ConnHandle<'io>, NetError>, exec_accept);
impl_direct_handler!([], Connect<'io>, Result<ConnHandle<'io>, NetError>, exec_connect);
impl_direct_handler!(['a], SendFrom<'io, 'a>, Result<DmaBuf<'a>, BufError<'a, NetError>>, exec_send_from);
impl_direct_handler!(['a], RecvInto<'io, 'a>, Result<FilledBuf<'a>, BufError<'a, NetError>>, exec_recv_into);
impl_direct_handler!([], CloseConn<'io>, Result<(), NetError>, exec_close_conn);
impl_direct_handler!([], Unlisten<'io>, Result<(), NetError>, exec_unlisten);
impl_direct_handler!([], Sleep, (), exec_sleep);
impl_direct_handler!(['a], FillRandom<'a>, Result<DmaBuf<'a>, BufError<'a, FsError>>, exec_fill_random);

// Unified request-union handlers delegating to the same small `exec_*` methods.
impl<'io, 'a, 'd, D: Driver> Handler<FsRequest<'io, 'a>> for DriverHandler<'io, 'd, D> {
    type Error = DriverHandlerError;

    fn perform<Out>(
        &mut self,
        request: FsRequest<'io, 'a>,
        deliver: impl FnOnce(Perform<FsAnswer<'io, 'a, '_>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = self.exec_fs(request)?;
        Ok(deliver(Perform::Answered(ans)))
    }
}

impl<'io, 'a, 'd, D: Driver> Handler<IoRequest<'io, 'a>> for DriverHandler<'io, 'd, D> {
    type Error = DriverHandlerError;

    fn perform<Out>(
        &mut self,
        request: IoRequest<'io, 'a>,
        deliver: impl FnOnce(Perform<IoAnswer<'io, 'a, '_>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let ans = self.exec_io(request)?;
        Ok(deliver(Perform::Answered(ans)))
    }
}
