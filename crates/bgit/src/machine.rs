//! Explicit typestate sans-IO machine for lock-protected atomic Git ref updates
//! (`.git/refs/heads/<branch>`) over [`blaze_core::request::FsRequest`]
//! (`docs/v2/plan/design/sans-io-machines.md`).
//!
//! Durability & lock sequence:
//! 1. `Lock(lock_path, Exclusive)` -> acquires linear `Lock<'io>` handle (`Contended` fails
//!    immediately with [`RefUpdateError::Locked`] without touching `ref_path`).
//! 2. `OpenWrite(lock_path)` -> acquires linear `File<'io, Unflushed>`.
//! 3. `WriteFrom(file, oid_buf)` -> writes 41-byte hex OID + `\n`.
//! 4. `Flush(file)` -> transitions to `File<'io, Flushed>` + [`FlushedProof`] (skipped under
//!    planted bug [`RefUpdatePlant::SkipFlushBeforeRename`]).
//! 5. `Close(file)` -> consumes linear `File`.
//! 6. `RenameFlushed(lock_path -> ref_path)` -> atomic ref flip.
//! 7. `SyncDir` -> makes directory entry durable.
//! 8. `Unlock(lock)` -> consumes linear `Lock<'io>` -> `Turn::Done(RefUpdated)`.
//!
//! Invariants:
//! - Two linear handles tracked simultaneously (`HandleSlot(0)` for `Lock<'io>`, `HandleSlot(1)`
//!   for `File<'io, _>`): on ANY error while holding an open `File` and/or `Lock`, the machine
//!   transitions through `ClosingFileOnError` and/or `UnlockingOnError` so ZERO handles leak
//!   (outside the explicit planted-bug control [`RefUpdatePlant::SkipUnlockOnError`]).
//! - Bounded cancellation cleanup: `on_cancel` returns [`CancelAction::Cleanup`] closing any open
//!   file and releasing the lock within `grace_max_steps = 2`.

#![blaze::sans_io]
#![forbid(unsafe_code)]

use core::fmt;

use blaze_core::handle::{Dir, File, Lock, ReadWrite, Unflushed};
use blaze_core::request::{
    ByteOffset, DmaBuf, FlushedProof, FsAnswer, FsError, FsRequest, LockOpts, LockOutcome,
    OpenOpts, RenameFlushedOpts, RenameUnsyncedOpts, WriteFromOpts,
};
use blaze_core::{FileName, LockMode};
use blaze_machine::{
    CancelAction, CancelReason, EdgeKind, GraphEdge, HandleOp, HandleSlot, Machine, MachineGraph,
    StateId, StateNode, Stopped, Turn,
};
use blaze_own::planted::{NoPlant, Planted, Plants};

/// Planted-bug switches for [`AtomicRefUpdateMachine`] DST controls (rules 43 & 53).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum RefUpdatePlant {
    #[default]
    None,
    /// Planted bug: skip `Flush` and `SyncDir`, closing the unflushed lock file and renaming via
    /// `RenameUnsynced` so a crash can lose or tear the updated ref.
    SkipFlushBeforeRename,
    /// Planted bug: return `Turn::Fail` on error without releasing the held linear `Lock<'io>`
    /// handle (caught by `DriverHandler::finish_task(ExitKind::Normal)`).
    SkipUnlockOnError,
}

impl Plants for RefUpdatePlant {}

/// Options for constructing [`AtomicRefUpdateMachine`] (`#[blaze::args]`, rule 83).
#[blaze::args]
pub struct RefUpdateOpts<'io, 'a> {
    /// Directory scope holding both `lock_path` and `ref_path`.
    pub dir: Dir<'io, ReadWrite>,
    /// Lock file path (e.g. `.git/refs/heads/main.lock`).
    pub lock_path: FileName,
    /// Final target ref path (e.g. `.git/refs/heads/main`).
    pub ref_path: FileName,
    /// Caller-owned DMA buffer holding the 41-byte hex OID + `\n` payload.
    pub oid_buf: DmaBuf<'a>,
}

/// Terminal success outcome of [`AtomicRefUpdateMachine`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RefUpdated {
    pub ref_path: FileName,
    pub bytes: u64,
}

/// Terminal failure outcome of [`AtomicRefUpdateMachine`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RefUpdateError {
    /// Another writer holds the exclusive advisory lock on `lock_path`.
    Locked { lock_path: FileName },
    /// Underlying filesystem error.
    Fs(FsError),
    /// External cancellation / deadline expiry after bounded cleanup.
    Cancelled(CancelReason),
}

impl fmt::Display for RefUpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RefUpdateError::Locked { lock_path } => {
                write!(f, "git ref update contended on lock {lock_path}")
            }
            RefUpdateError::Fs(err) => write!(f, "git ref update failed: {err}"),
            RefUpdateError::Cancelled(reason) => {
                write!(f, "git ref update cancelled ({reason:?})")
            }
        }
    }
}

enum State<'io, 'a> {
    Init {
        lock_path: FileName,
        ref_path: FileName,
        oid_buf: DmaBuf<'a>,
    },
    Locking {
        lock_path: FileName,
        ref_path: FileName,
        oid_buf: DmaBuf<'a>,
    },
    Opening {
        lock_path: FileName,
        ref_path: FileName,
        oid_buf: DmaBuf<'a>,
        lock: Lock<'io>,
    },
    Writing {
        lock_path: FileName,
        ref_path: FileName,
        lock: Lock<'io>,
        file: File<'io, Unflushed>,
    },
    Flushing {
        ref_path: FileName,
        lock: Lock<'io>,
    },
    ClosingBeforeRename {
        ref_path: FileName,
        lock: Lock<'io>,
        proof: Option<FlushedProof>,
        unsynced_from: Option<FileName>,
    },
    Renaming {
        ref_path: FileName,
        lock: Lock<'io>,
        sync_dir: bool,
    },
    SyncingDir {
        ref_path: FileName,
        lock: Lock<'io>,
    },
    Unlocking {
        ref_path: FileName,
    },
    ClosingFileOnError {
        lock: Lock<'io>,
        err: FsError,
    },
    UnlockingOnError {
        err: FsError,
    },
    ClosingFileOnCancel {
        lock: Lock<'io>,
        reason: CancelReason,
    },
    UnlockingOnCancel {
        reason: CancelReason,
    },
}

/// Explicit typestate sans-IO machine performing a lock-protected atomic Git ref update over
/// [`FsRequest`].
pub struct AtomicRefUpdateMachine<'io, 'a, P: Planted<RefUpdatePlant> = NoPlant> {
    dir: Dir<'io, ReadWrite>,
    bytes: u64,
    state: State<'io, 'a>,
    plant: P,
}

impl<'io, 'a> AtomicRefUpdateMachine<'io, 'a, NoPlant> {
    /// Creates a production [`AtomicRefUpdateMachine`] with zero planted bugs.
    #[inline]
    pub fn new(opts: RefUpdateOpts<'io, 'a>) -> Self {
        let bytes = opts.oid_buf.len() as u64;
        Self {
            dir: opts.dir,
            bytes,
            state: State::Init {
                lock_path: opts.lock_path,
                ref_path: opts.ref_path,
                oid_buf: opts.oid_buf,
            },
            plant: NoPlant,
        }
    }
}

impl<'io, 'a, P: Planted<RefUpdatePlant>> AtomicRefUpdateMachine<'io, 'a, P> {
    /// Creates an [`AtomicRefUpdateMachine`] carrying a planted-bug seam for DST controls.
    #[inline]
    pub fn with_plant(opts: RefUpdateOpts<'io, 'a>, plant: P) -> Self {
        let bytes = opts.oid_buf.len() as u64;
        Self {
            dir: opts.dir,
            bytes,
            state: State::Init {
                lock_path: opts.lock_path,
                ref_path: opts.ref_path,
                oid_buf: opts.oid_buf,
            },
            plant,
        }
    }

    fn unlock_or_fail_on_error(
        dir: Dir<'io, ReadWrite>,
        bytes: u64,
        plant: P,
        lock: Lock<'io>,
        err: FsError,
    ) -> Turn<Self, FsRequest<'io, 'a>, RefUpdated, RefUpdateError> {
        if plant.has(RefUpdatePlant::SkipUnlockOnError) {
            drop(lock);
            Turn::Fail(RefUpdateError::Fs(err))
        } else {
            Turn::Ask {
                request: FsRequest::Unlock(lock.unlock()),
                next: Self { dir, bytes, state: State::UnlockingOnError { err }, plant },
            }
        }
    }
}

impl<'io, 'a, P: Planted<RefUpdatePlant>> Machine<()> for AtomicRefUpdateMachine<'io, 'a, P> {
    type Req = FsRequest<'io, 'a>;
    type Out = RefUpdated;
    type Err = RefUpdateError;

    fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        let Self { dir, bytes, state, plant } = self;
        match state {
            State::Init { lock_path, ref_path, oid_buf } => {
                let req = dir.lock(LockOpts {
                    path: lock_path.clone(),
                    mode: LockMode::Exclusive,
                });
                Turn::Ask {
                    request: FsRequest::Lock(req),
                    next: Self {
                        dir,
                        bytes,
                        state: State::Locking { lock_path, ref_path, oid_buf },
                        plant,
                    },
                }
            }
            State::Locking { .. }
            | State::Opening { .. }
            | State::Writing { .. }
            | State::Flushing { .. }
            | State::ClosingBeforeRename { .. }
            | State::Renaming { .. }
            | State::SyncingDir { .. }
            | State::Unlocking { .. }
            | State::ClosingFileOnError { .. }
            | State::UnlockingOnError { .. }
            | State::ClosingFileOnCancel { .. }
            | State::UnlockingOnCancel { .. } => {
                unreachable!("AtomicRefUpdateMachine::start called on non-Init state")
            }
        }
    }

    fn resume(
        self,
        answer: FsAnswer<'io, 'a, '_>,
        _cx: &mut (),
    ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        let Self { dir, bytes, state, plant } = self;
        match (state, answer) {
            (
                State::Locking { lock_path, ref_path, oid_buf },
                FsAnswer::Locked(Ok(LockOutcome::Acquired(lock))),
            ) => {
                let req = dir.open_write(OpenOpts {
                    path: lock_path.clone(),
                    create: true,
                    truncate: true,
                });
                Turn::Ask {
                    request: FsRequest::OpenWrite(req),
                    next: Self {
                        dir,
                        bytes,
                        state: State::Opening { lock_path, ref_path, oid_buf, lock },
                        plant,
                    },
                }
            }
            (
                State::Locking { lock_path, oid_buf, .. },
                FsAnswer::Locked(Ok(LockOutcome::Contended)),
            ) => {
                oid_buf.release();
                Turn::Fail(RefUpdateError::Locked { lock_path })
            }
            (State::Locking { oid_buf, .. }, FsAnswer::Locked(Err(err))) => {
                oid_buf.release();
                Turn::Fail(RefUpdateError::Fs(err))
            }
            (
                State::Opening { lock_path, ref_path, oid_buf, lock },
                FsAnswer::OpenedWrite(Ok(file)),
            ) => {
                let req = file.write_from(WriteFromOpts { at: ByteOffset(0), buf: oid_buf });
                Turn::Ask {
                    request: FsRequest::WriteFrom(req),
                    next: Self {
                        dir,
                        bytes,
                        state: State::Writing { lock_path, ref_path, lock, file },
                        plant,
                    },
                }
            }
            (State::Opening { oid_buf, lock, .. }, FsAnswer::OpenedWrite(Err(err))) => {
                oid_buf.release();
                Self::unlock_or_fail_on_error(dir, bytes, plant, lock, err)
            }
            (
                State::Writing { lock_path, ref_path, lock, file },
                FsAnswer::Written(Ok(buf)),
            ) => {
                buf.release();
                if plant.has(RefUpdatePlant::SkipFlushBeforeRename) {
                    Turn::Ask {
                        request: FsRequest::Close(file.close()),
                        next: Self {
                            dir,
                            bytes,
                            state: State::ClosingBeforeRename {
                                ref_path,
                                lock,
                                proof: None,
                                unsynced_from: Some(lock_path),
                            },
                            plant,
                        },
                    }
                } else {
                    Turn::Ask {
                        request: FsRequest::Flush(file.flush()),
                        next: Self {
                            dir,
                            bytes,
                            state: State::Flushing { ref_path, lock },
                            plant,
                        },
                    }
                }
            }
            (State::Writing { lock, file, .. }, FsAnswer::Written(Err(buf_err))) => {
                buf_err.buf.release();
                Turn::Ask {
                    request: FsRequest::Close(file.close()),
                    next: Self {
                        dir,
                        bytes,
                        state: State::ClosingFileOnError { lock, err: buf_err.error },
                        plant,
                    },
                }
            }
            (State::Flushing { ref_path, lock }, FsAnswer::Flushed(Ok((file, proof)))) => {
                Turn::Ask {
                    request: FsRequest::Close(file.close()),
                    next: Self {
                        dir,
                        bytes,
                        state: State::ClosingBeforeRename {
                            ref_path,
                            lock,
                            proof: Some(proof),
                            unsynced_from: None,
                        },
                        plant,
                    },
                }
            }
            (State::Flushing { lock, .. }, FsAnswer::Flushed(Err((file, err)))) => Turn::Ask {
                request: FsRequest::Close(file.close()),
                next: Self {
                    dir,
                    bytes,
                    state: State::ClosingFileOnError { lock, err },
                    plant,
                },
            },
            (
                State::ClosingBeforeRename { ref_path, lock, proof, unsynced_from },
                FsAnswer::Closed(Ok(())),
            ) => {
                if let Some(flushed) = proof {
                    let req =
                        dir.rename_flushed(RenameFlushedOpts { flushed, to: ref_path.clone() });
                    Turn::Ask {
                        request: FsRequest::RenameFlushed(req),
                        next: Self {
                            dir,
                            bytes,
                            state: State::Renaming { ref_path, lock, sync_dir: true },
                            plant,
                        },
                    }
                } else {
                    let from = unsynced_from
                        .expect("planted SkipFlushBeforeRename always carries unsynced_from");
                    let req =
                        dir.rename_unsynced(RenameUnsyncedOpts { from, to: ref_path.clone() });
                    Turn::Ask {
                        request: FsRequest::RenameUnsynced(req),
                        next: Self {
                            dir,
                            bytes,
                            state: State::Renaming { ref_path, lock, sync_dir: false },
                            plant,
                        },
                    }
                }
            }
            (State::ClosingBeforeRename { lock, .. }, FsAnswer::Closed(Err(err)))
            | (State::Renaming { lock, .. }, FsAnswer::Renamed(Err(err)))
            | (State::SyncingDir { lock, .. }, FsAnswer::SyncedDir(Err(err))) => {
                Self::unlock_or_fail_on_error(dir, bytes, plant, lock, err)
            }
            (State::Renaming { ref_path, lock, sync_dir }, FsAnswer::Renamed(Ok(()))) => {
                if sync_dir {
                    Turn::Ask {
                        request: FsRequest::SyncDir(dir.sync_dir()),
                        next: Self {
                            dir,
                            bytes,
                            state: State::SyncingDir { ref_path, lock },
                            plant,
                        },
                    }
                } else {
                    Turn::Ask {
                        request: FsRequest::Unlock(lock.unlock()),
                        next: Self { dir, bytes, state: State::Unlocking { ref_path }, plant },
                    }
                }
            }
            (State::SyncingDir { ref_path, lock }, FsAnswer::SyncedDir(Ok(()))) => Turn::Ask {
                request: FsRequest::Unlock(lock.unlock()),
                next: Self { dir, bytes, state: State::Unlocking { ref_path }, plant },
            },
            (State::Unlocking { ref_path }, FsAnswer::Unlocked(Ok(()))) => {
                Turn::Done(RefUpdated { ref_path, bytes })
            }
            (State::Unlocking { .. }, FsAnswer::Unlocked(Err(err))) => {
                Turn::Fail(RefUpdateError::Fs(err))
            }
            (State::ClosingFileOnError { lock, err }, FsAnswer::Closed(_)) => {
                Self::unlock_or_fail_on_error(dir, bytes, plant, lock, err)
            }
            (State::UnlockingOnError { err }, FsAnswer::Unlocked(_)) => {
                Turn::Fail(RefUpdateError::Fs(err))
            }
            (State::ClosingFileOnCancel { lock, reason }, FsAnswer::Closed(_)) => Turn::Ask {
                request: FsRequest::Unlock(lock.unlock()),
                next: Self { dir, bytes, state: State::UnlockingOnCancel { reason }, plant },
            },
            (State::UnlockingOnCancel { reason }, FsAnswer::Unlocked(_)) => {
                Turn::Fail(RefUpdateError::Cancelled(reason))
            }
            _ => unreachable!("FsAnswer always matches active waiting state"),
        }
    }

    fn on_cancel(
        self,
        stopped: Stopped,
        _cx: &mut (),
    ) -> CancelAction<Turn<Self, Self::Req, Self::Out, Self::Err>> {
        let Self { dir, bytes, state, plant } = self;
        match state {
            State::Init { oid_buf, .. } | State::Locking { oid_buf, .. } => {
                oid_buf.release();
                CancelAction::Abandon
            }
            State::Opening { oid_buf, lock, .. } => {
                oid_buf.release();
                CancelAction::Cleanup(Turn::Ask {
                    request: FsRequest::Unlock(lock.unlock()),
                    next: Self {
                        dir,
                        bytes,
                        state: State::UnlockingOnCancel { reason: stopped.reason },
                        plant,
                    },
                })
            }
            State::Writing { lock, file, .. } => CancelAction::Cleanup(Turn::Ask {
                request: FsRequest::Close(file.close()),
                next: Self {
                    dir,
                    bytes,
                    state: State::ClosingFileOnCancel { lock, reason: stopped.reason },
                    plant,
                },
            }),
            State::ClosingBeforeRename { lock, .. }
            | State::Renaming { lock, .. }
            | State::SyncingDir { lock, .. }
            | State::ClosingFileOnError { lock, .. } => CancelAction::Cleanup(Turn::Ask {
                request: FsRequest::Unlock(lock.unlock()),
                next: Self {
                    dir,
                    bytes,
                    state: State::UnlockingOnCancel { reason: stopped.reason },
                    plant,
                },
            }),
            State::Flushing { .. }
            | State::Unlocking { .. }
            | State::UnlockingOnError { .. }
            | State::ClosingFileOnCancel { .. }
            | State::UnlockingOnCancel { .. } => CancelAction::Abandon,
        }
    }
}

/// Reflected state graph of [`AtomicRefUpdateMachine`], tracking BOTH linear handle slots
/// (`HandleSlot(0)` for `Lock<'io>`, `HandleSlot(1)` for `File<'io, _>`) and verified by
/// [`blaze_machine::verify_graph`].
pub fn atomic_ref_update_graph() -> MachineGraph<'static> {
    const INIT: StateId = StateId(0);
    const LOCKING: StateId = StateId(1);
    const OPENING: StateId = StateId(2);
    const WRITING: StateId = StateId(3);
    const FLUSHING: StateId = StateId(4);
    const CLOSING_BEFORE_RENAME: StateId = StateId(5);
    const RENAMING: StateId = StateId(6);
    const SYNCING_DIR: StateId = StateId(7);
    const UNLOCKING: StateId = StateId(8);
    const CLOSING_FILE_ON_ERROR: StateId = StateId(9);
    const UNLOCKING_ON_ERROR: StateId = StateId(10);
    const CLOSING_FILE_ON_CANCEL: StateId = StateId(11);
    const UNLOCKING_ON_CANCEL: StateId = StateId(12);

    const LOCK_HANDLE: HandleSlot = HandleSlot(0);
    const FILE_HANDLE: HandleSlot = HandleSlot(1);

    static STATES: [StateNode; 13] = [
        StateNode { id: INIT, name: "Init", loop_state: false, initial_handles: 0 },
        StateNode { id: LOCKING, name: "Locking", loop_state: false, initial_handles: 0 },
        StateNode { id: OPENING, name: "Opening", loop_state: false, initial_handles: 0 },
        StateNode { id: WRITING, name: "Writing", loop_state: false, initial_handles: 0 },
        StateNode { id: FLUSHING, name: "Flushing", loop_state: false, initial_handles: 0 },
        StateNode {
            id: CLOSING_BEFORE_RENAME,
            name: "ClosingBeforeRename",
            loop_state: false,
            initial_handles: 0,
        },
        StateNode { id: RENAMING, name: "Renaming", loop_state: false, initial_handles: 0 },
        StateNode { id: SYNCING_DIR, name: "SyncingDir", loop_state: false, initial_handles: 0 },
        StateNode { id: UNLOCKING, name: "Unlocking", loop_state: false, initial_handles: 0 },
        StateNode {
            id: CLOSING_FILE_ON_ERROR,
            name: "ClosingFileOnError",
            loop_state: false,
            initial_handles: 0,
        },
        StateNode {
            id: UNLOCKING_ON_ERROR,
            name: "UnlockingOnError",
            loop_state: false,
            initial_handles: 0,
        },
        StateNode {
            id: CLOSING_FILE_ON_CANCEL,
            name: "ClosingFileOnCancel",
            loop_state: false,
            initial_handles: 0,
        },
        StateNode {
            id: UNLOCKING_ON_CANCEL,
            name: "UnlockingOnCancel",
            loop_state: false,
            initial_handles: 0,
        },
    ];

    static EDGES: [GraphEdge; 34] = [
        GraphEdge {
            from: INIT,
            kind: EdgeKind::Ask { to: LOCKING },
            handle_op: HandleOp::None,
            label: "acquire_lock",
        },
        GraphEdge {
            from: LOCKING,
            kind: EdgeKind::Ask { to: OPENING },
            handle_op: HandleOp::Open(LOCK_HANDLE),
            label: "lock_acquired_open_write",
        },
        GraphEdge {
            from: LOCKING,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "lock_contended_or_err",
        },
        GraphEdge {
            from: LOCKING,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "lock_cancel_abandon",
        },
        GraphEdge {
            from: OPENING,
            kind: EdgeKind::Ask { to: WRITING },
            handle_op: HandleOp::Open(FILE_HANDLE),
            label: "open_ok_write_oid",
        },
        GraphEdge {
            from: OPENING,
            kind: EdgeKind::Ask { to: UNLOCKING_ON_ERROR },
            handle_op: HandleOp::Close(LOCK_HANDLE),
            label: "open_err_unlock",
        },
        GraphEdge {
            from: OPENING,
            kind: EdgeKind::CancelCleanup { to: UNLOCKING_ON_CANCEL },
            handle_op: HandleOp::Close(LOCK_HANDLE),
            label: "open_cancel_unlock",
        },
        GraphEdge {
            from: WRITING,
            kind: EdgeKind::Ask { to: FLUSHING },
            handle_op: HandleOp::None,
            label: "write_ok_flush",
        },
        GraphEdge {
            from: WRITING,
            kind: EdgeKind::Ask { to: CLOSING_BEFORE_RENAME },
            handle_op: HandleOp::Close(FILE_HANDLE),
            label: "write_ok_planted_close",
        },
        GraphEdge {
            from: WRITING,
            kind: EdgeKind::Ask { to: CLOSING_FILE_ON_ERROR },
            handle_op: HandleOp::Close(FILE_HANDLE),
            label: "write_err_close_file",
        },
        GraphEdge {
            from: WRITING,
            kind: EdgeKind::CancelCleanup { to: CLOSING_FILE_ON_CANCEL },
            handle_op: HandleOp::Close(FILE_HANDLE),
            label: "write_cancel_close_file",
        },
        GraphEdge {
            from: FLUSHING,
            kind: EdgeKind::Ask { to: CLOSING_BEFORE_RENAME },
            handle_op: HandleOp::Close(FILE_HANDLE),
            label: "flush_ok_close_file",
        },
        GraphEdge {
            from: FLUSHING,
            kind: EdgeKind::Ask { to: CLOSING_FILE_ON_ERROR },
            handle_op: HandleOp::Close(FILE_HANDLE),
            label: "flush_err_close_file",
        },
        GraphEdge {
            from: FLUSHING,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "flush_cancel_abandon",
        },
        GraphEdge {
            from: CLOSING_BEFORE_RENAME,
            kind: EdgeKind::Ask { to: RENAMING },
            handle_op: HandleOp::None,
            label: "close_ok_rename",
        },
        GraphEdge {
            from: CLOSING_BEFORE_RENAME,
            kind: EdgeKind::Ask { to: UNLOCKING_ON_ERROR },
            handle_op: HandleOp::Close(LOCK_HANDLE),
            label: "close_err_unlock",
        },
        GraphEdge {
            from: CLOSING_BEFORE_RENAME,
            kind: EdgeKind::CancelCleanup { to: UNLOCKING_ON_CANCEL },
            handle_op: HandleOp::Close(LOCK_HANDLE),
            label: "close_cancel_unlock",
        },
        GraphEdge {
            from: RENAMING,
            kind: EdgeKind::Ask { to: SYNCING_DIR },
            handle_op: HandleOp::None,
            label: "rename_ok_sync_dir",
        },
        GraphEdge {
            from: RENAMING,
            kind: EdgeKind::Ask { to: UNLOCKING },
            handle_op: HandleOp::Close(LOCK_HANDLE),
            label: "rename_ok_planted_unlock",
        },
        GraphEdge {
            from: RENAMING,
            kind: EdgeKind::Ask { to: UNLOCKING_ON_ERROR },
            handle_op: HandleOp::Close(LOCK_HANDLE),
            label: "rename_err_unlock",
        },
        GraphEdge {
            from: RENAMING,
            kind: EdgeKind::CancelCleanup { to: UNLOCKING_ON_CANCEL },
            handle_op: HandleOp::Close(LOCK_HANDLE),
            label: "rename_cancel_unlock",
        },
        GraphEdge {
            from: SYNCING_DIR,
            kind: EdgeKind::Ask { to: UNLOCKING },
            handle_op: HandleOp::Close(LOCK_HANDLE),
            label: "sync_dir_ok_unlock",
        },
        GraphEdge {
            from: SYNCING_DIR,
            kind: EdgeKind::Ask { to: UNLOCKING_ON_ERROR },
            handle_op: HandleOp::Close(LOCK_HANDLE),
            label: "sync_dir_err_unlock",
        },
        GraphEdge {
            from: SYNCING_DIR,
            kind: EdgeKind::CancelCleanup { to: UNLOCKING_ON_CANCEL },
            handle_op: HandleOp::Close(LOCK_HANDLE),
            label: "sync_dir_cancel_unlock",
        },
        GraphEdge {
            from: UNLOCKING,
            kind: EdgeKind::Done,
            handle_op: HandleOp::None,
            label: "unlock_ok_done",
        },
        GraphEdge {
            from: UNLOCKING,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "unlock_err_fail",
        },
        GraphEdge {
            from: UNLOCKING,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "unlock_cancel_abandon",
        },
        GraphEdge {
            from: CLOSING_FILE_ON_ERROR,
            kind: EdgeKind::Ask { to: UNLOCKING_ON_ERROR },
            handle_op: HandleOp::Close(LOCK_HANDLE),
            label: "error_close_then_unlock",
        },
        GraphEdge {
            from: CLOSING_FILE_ON_ERROR,
            kind: EdgeKind::CancelCleanup { to: UNLOCKING_ON_CANCEL },
            handle_op: HandleOp::Close(LOCK_HANDLE),
            label: "error_close_cancel_unlock",
        },
        GraphEdge {
            from: UNLOCKING_ON_ERROR,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "error_unlock_fail",
        },
        GraphEdge {
            from: UNLOCKING_ON_ERROR,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "error_unlock_abandon",
        },
        GraphEdge {
            from: CLOSING_FILE_ON_CANCEL,
            kind: EdgeKind::Ask { to: UNLOCKING_ON_CANCEL },
            handle_op: HandleOp::Close(LOCK_HANDLE),
            label: "cancel_close_then_unlock",
        },
        GraphEdge {
            from: CLOSING_FILE_ON_CANCEL,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "cancel_close_abandon",
        },
        GraphEdge {
            from: UNLOCKING_ON_CANCEL,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "cancel_unlock_done",
        },
    ];

    MachineGraph {
        name: "AtomicRefUpdate",
        entry: INIT,
        grace_max_steps: 2,
        states: &STATES,
        edges: &EDGES,
    }
}

#[cfg(test)]
mod tests {
    use blaze_core::handle::{ExitKind, TaskId};
    use blaze_core::request::{DmaBuf, DriverHandlerError, FsError, SimHandler};
    use blaze_core::sim::{Faults, Sim};
    use blaze_core::{FileName, LockMode};
    use blaze_machine::{DriveError, DriveOpts, drive, emit_lean, verify_graph};
    use blaze_own::{FixedString, with_brand};

    use super::{
        AtomicRefUpdateMachine, RefUpdateError, RefUpdateOpts, RefUpdatePlant, RefUpdated,
        atomic_ref_update_graph,
    };

    const OID_LINE: &[u8] = b"0123456789abcdef0123456789abcdef01234567\n";

    #[test]
    fn atomic_ref_update_graph_verifies_two_linear_handles_and_emits_lean() {
        let graph = atomic_ref_update_graph();
        assert_eq!(verify_graph(&graph), Ok(()));

        let mut lean = FixedString::<16384>::new();
        assert_eq!(emit_lean(&graph, &mut lean), Ok(()));
        let text = lean.text().expect("Lean output fits in 16384 bytes");
        assert!(text.contains("namespace AtomicRefUpdate"), "missing namespace:\n{text}");
        assert!(text.contains("inductive State where"), "missing State:\n{text}");
        // Verify both linear handle slots (`0` = Lock, `1` = File) appear in emitted transitions.
        assert!(text.contains("(1 <<< 0)"), "missing Lock HandleSlot(0) in Lean:\n{text}");
        assert!(text.contains("(1 <<< 1)"), "missing File HandleSlot(1) in Lean:\n{text}");
        assert!(
            text.contains("theorem verified_machine_no_leak"),
            "missing verified_machine_no_leak:\n{text}"
        );
        assert!(text.contains("end AtomicRefUpdate"), "missing end namespace:\n{text}");
    }

    #[test]
    fn atomic_ref_update_commits_durably_across_crash_over_sim_handler() {
        assert_eq!(OID_LINE.len(), 41);
        let alloc = blaze_own::Alloc::global();
        let lock_path = FileName::new(".git/refs/heads/main.lock").expect("valid lock path");
        let ref_path = FileName::new(".git/refs/heads/main").expect("valid ref path");

        // 1. Contended lock fails immediately without touching `ref_path` and leaks zero handles.
        let mut contended_sim = Sim::new(1, Faults::default());
        contended_sim.hold_lock(&lock_path, LockMode::Exclusive);
        with_brand(|brand| {
            let mut handler = SimHandler::new(&mut contended_sim, TaskId(1), brand, alloc);
            let root = handler.root();
            let oid_buf = DmaBuf::from_slice_in(OID_LINE, alloc);
            let machine = AtomicRefUpdateMachine::new(RefUpdateOpts {
                dir: root,
                lock_path: lock_path.clone(),
                ref_path: ref_path.clone(),
                oid_buf,
            });
            let res = drive(&mut handler, &mut (), machine, DriveOpts { max_steps: 16 });
            assert_eq!(
                res,
                Err(DriveError::Machine(RefUpdateError::Locked { lock_path: lock_path.clone() }))
            );
            assert_eq!(handler.finish_task(ExitKind::Normal), Ok(0));
        });
        assert!(!contended_sim.exists(&ref_path), "contended update must not create ref_path");

        // 2. Durable happy path: commit via SimHandler, power-cut with `sim.crash()`, verify exact
        //    41-byte OID survives AND zero handles leaked.
        let mut sim = Sim::new(42, Faults::default());
        with_brand(|brand| {
            let mut handler = SimHandler::new(&mut sim, TaskId(2), brand, alloc);
            let root = handler.root();
            let oid_buf = DmaBuf::from_slice_in(OID_LINE, alloc);
            let machine = AtomicRefUpdateMachine::new(RefUpdateOpts {
                dir: root,
                lock_path: lock_path.clone(),
                ref_path: ref_path.clone(),
                oid_buf,
            });
            let outcome = drive(&mut handler, &mut (), machine, DriveOpts { max_steps: 16 });
            assert_eq!(outcome, Ok(RefUpdated { ref_path: ref_path.clone(), bytes: 41 }));
            assert_eq!(handler.finish_task(ExitKind::Normal), Ok(0));
        });

        sim.crash();
        assert!(sim.exists(&ref_path), "committed git ref missing after crash");
        assert_eq!(sim.contents(&ref_path), OID_LINE);
        assert!(!sim.exists(&lock_path), "lock file should be renamed away after commit");

        // 3. Planted-bug control: `SkipFlushBeforeRename` loses or tears durability on `Sim::crash`.
        let mut caught_durability_loss = false;
        for seed in 0u64..32 {
            let mut planted_sim = Sim::new(seed, Faults::default());
            with_brand(|brand| {
                let mut handler = SimHandler::new(&mut planted_sim, TaskId(3), brand, alloc);
                let root = handler.root();
                let oid_buf = DmaBuf::from_slice_in(OID_LINE, alloc);
                let machine = AtomicRefUpdateMachine::with_plant(
                    RefUpdateOpts {
                        dir: root,
                        lock_path: lock_path.clone(),
                        ref_path: ref_path.clone(),
                        oid_buf,
                    },
                    RefUpdatePlant::SkipFlushBeforeRename,
                );
                let outcome = drive(&mut handler, &mut (), machine, DriveOpts { max_steps: 16 });
                assert_eq!(outcome, Ok(RefUpdated { ref_path: ref_path.clone(), bytes: 41 }));
                assert_eq!(handler.finish_task(ExitKind::Normal), Ok(0));
            });
            planted_sim.crash();
            if !planted_sim.exists(&ref_path) || planted_sim.contents(&ref_path) != OID_LINE {
                caught_durability_loss = true;
                break;
            }
        }
        assert!(
            caught_durability_loss,
            "SkipFlushBeforeRename planted control must lose or tear ref on Sim::crash"
        );
    }

    #[test]
    fn atomic_ref_update_closes_and_unlocks_on_fault_with_planted_leak_control() {
        let alloc = blaze_own::Alloc::global();
        let dir_name = FileName::new(".git/refs/heads").expect("valid dir name");
        let lock_path = FileName::new(".git/refs/heads/feature.lock").expect("valid lock path");
        let ref_path = FileName::new(".git/refs/heads/feature").expect("valid ref path");

        // 1. Normal machine: `Sim::fill(dir, 0)` lets `Lock` and `OpenWrite` succeed and faults
        //    `WriteFrom` with `NoSpace`. The machine closes its `File` AND unlocks its `Lock`
        //    before failing, so `finish_task(ExitKind::Normal)` reports zero leaked handles.
        let mut sim = Sim::new(7, Faults::default());
        sim.fill(dir_name.clone(), 0);
        with_brand(|brand| {
            let mut handler = SimHandler::new(&mut sim, TaskId(10), brand, alloc);
            let root = handler.root();
            let oid_buf = DmaBuf::from_slice_in(OID_LINE, alloc);
            let machine = AtomicRefUpdateMachine::with_plant(
                RefUpdateOpts {
                    dir: root,
                    lock_path: lock_path.clone(),
                    ref_path: ref_path.clone(),
                    oid_buf,
                },
                RefUpdatePlant::None,
            );
            let res = drive(&mut handler, &mut (), machine, DriveOpts { max_steps: 16 });
            assert!(
                matches!(
                    res,
                    Err(DriveError::Machine(RefUpdateError::Fs(FsError::NoSpace { .. })))
                ),
                "expected NoSpace fault on WriteFrom, got {res:?}"
            );
            assert_eq!(handler.finish_task(ExitKind::Normal), Ok(0));
        });
        assert!(!sim.exists(&ref_path), "failed ref update must not flip ref_path");

        // 2. Planted-bug control: `RefUpdatePlant::SkipUnlockOnError` closes the file but drops
        //    the linear `Lock<'io>` without issuing `Unlock`, so `finish_task(ExitKind::Normal)`
        //    catches the leaked handle!
        let mut planted_sim = Sim::new(7, Faults::default());
        planted_sim.fill(dir_name, 0);
        with_brand(|brand| {
            let mut handler = SimHandler::new(&mut planted_sim, TaskId(11), brand, alloc);
            let root = handler.root();
            let oid_buf = DmaBuf::from_slice_in(OID_LINE, alloc);
            let machine = AtomicRefUpdateMachine::with_plant(
                RefUpdateOpts {
                    dir: root,
                    lock_path: lock_path.clone(),
                    ref_path: ref_path.clone(),
                    oid_buf,
                },
                RefUpdatePlant::SkipUnlockOnError,
            );
            let res = drive(&mut handler, &mut (), machine, DriveOpts { max_steps: 16 });
            assert!(
                matches!(
                    res,
                    Err(DriveError::Machine(RefUpdateError::Fs(FsError::NoSpace { .. })))
                ),
                "expected NoSpace fault on WriteFrom, got {res:?}"
            );
            let finish = handler.finish_task(ExitKind::Normal);
            assert!(
                matches!(
                    finish,
                    Err(DriverHandlerError::LeakedHandles(leaked))
                        if leaked.task == TaskId(11) && leaked.count == 1
                ),
                "expected DriverHandlerError::LeakedHandles for SkipUnlockOnError, got {finish:?}"
            );
        });
    }
}
