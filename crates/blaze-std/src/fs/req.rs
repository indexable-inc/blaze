//! Zero-alloc typed linear filesystem requests (`blaze_core::request`), typestate handles
//! (`blaze_core::handle`), and verified `#![blaze::sans_io]` filesystem machines
//! (`AtomicPublishMachine`).

#![blaze::sans_io]

use core::fmt;

use blaze_core::FileName;
use blaze_machine::{
    CancelAction, CancelReason, EdgeKind, GraphEdge, HandleOp, HandleSlot, Machine, MachineGraph,
    StateId, StateNode, Stopped, Turn,
};

pub use blaze_core::handle::{
    Dir, ExitKind, File, FileId, FixedFileSlot, Flushed, Lock, ReadOnly, ReadWrite, TaskId,
    Unflushed,
};
pub use blaze_core::request::{
    BufError, ByteLen, ByteOffset, Close, CopyData, CopyDataOpts, DirectFsBackend, DirectFsHandler,
    DirectFsHandlerError, DmaBuf, FilledBuf, FixedBufIndex, Flush, FlushedProof, FsAnswer, FsError,
    FsRequest, InMemoryDirectFs, LENT_READ_MAX, List, ListOpts, Lock as LockReq, LockOpts,
    LockOutcome, MakeDir, MaxBytes, NativeDirectFs, OpenOpts, OpenRead, OpenWrite, Read, ReadAll,
    ReadAllOpts, ReadInto, ReadIntoOpts, ReadOpts, Remove, RemoveDir, RenameFlushed,
    RenameFlushedOpts, RenameUnsynced, RenameUnsyncedOpts, Stat, SyncDir, Truncate, TruncateOpts,
    Unlock, WriteDurable, WriteDurableOpts, WriteFrom, WriteFromOpts,
};

/// Options for [`AtomicPublishMachine::new`].
#[blaze::args]
pub struct AtomicPublishOpts<'io, 'a> {
    pub dir: Dir<'io, ReadWrite>,
    pub tmp: FileName,
    pub target: FileName,
    pub buf: DmaBuf<'a>,
}

/// Typed failure returned by [`AtomicPublishMachine`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AtomicPublishError {
    /// An underlying filesystem step failed (any open temporary file handle was closed first).
    Fs(FsError),
    /// Publish was cancelled or exceeded its deadline and finished bounded cleanup (`Close` +
    /// `Remove`).
    Stopped(CancelReason),
}

impl fmt::Display for AtomicPublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AtomicPublishError::Fs(err) => write!(f, "atomic publish failed: {err}"),
            AtomicPublishError::Stopped(reason) => {
                write!(f, "atomic publish stopped ({reason:?}) after cleanup")
            }
        }
    }
}

impl core::error::Error for AtomicPublishError {}

enum AtomicPublishState<'io, 'a> {
    Start {
        dir: Dir<'io, ReadWrite>,
        tmp: FileName,
        target: FileName,
        buf: DmaBuf<'a>,
    },
    Opening {
        dir: Dir<'io, ReadWrite>,
        tmp: FileName,
        target: FileName,
        buf: DmaBuf<'a>,
    },
    Writing {
        dir: Dir<'io, ReadWrite>,
        tmp: FileName,
        target: FileName,
        file: File<'io, Unflushed>,
    },
    Flushing {
        dir: Dir<'io, ReadWrite>,
        tmp: FileName,
        target: FileName,
    },
    ClosingBeforeRename {
        dir: Dir<'io, ReadWrite>,
        tmp: FileName,
        target: FileName,
        proof: FlushedProof,
    },
    Renaming {
        dir: Dir<'io, ReadWrite>,
    },
    SyncingDir,
    ClosingToCleanup {
        dir: Dir<'io, ReadWrite>,
        tmp: FileName,
        err: AtomicPublishError,
    },
    RemovingTmp {
        err: AtomicPublishError,
    },
}

/// Explicit sans-IO [`Machine`] that durably publishes bytes to a target path via:
/// `OpenWrite(tmp) -> WriteFrom(tmp, buf) -> Flush(tmp) -> Close(tmp) -> RenameFlushed(tmp -> target) -> SyncDir`.
///
/// Closes its linear [`File`] and removes `tmp` on every write/flush error path, and implements
/// bounded `on_cancel` cleanup (`Close` -> `Remove(tmp)`). Its state graph is returned by
/// [`atomic_publish_graph`] and verified by [`blaze_machine::verify_graph`].
pub struct AtomicPublishMachine<'io, 'a>(AtomicPublishState<'io, 'a>);

impl<'io, 'a> AtomicPublishMachine<'io, 'a> {
    /// Creates a new durable atomic-publish machine ready to be driven by `Cx::drive_machine` or
    /// `blaze_machine::drive`.
    #[inline]
    pub fn new(opts: AtomicPublishOpts<'io, 'a>) -> Self {
        Self(AtomicPublishState::Start {
            dir: opts.dir,
            tmp: opts.tmp,
            target: opts.target,
            buf: opts.buf,
        })
    }
}

impl<'io, 'a> Machine for AtomicPublishMachine<'io, 'a> {
    type Req = FsRequest<'io, 'a>;
    type Out = ();
    type Err = AtomicPublishError;

    fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match self.0 {
            AtomicPublishState::Start { dir, tmp, target, buf } => {
                let req = dir.open_write(OpenOpts {
                    path: tmp.clone(),
                    create: true,
                    truncate: true,
                });
                Turn::Ask {
                    request: FsRequest::OpenWrite(req),
                    next: Self(AtomicPublishState::Opening { dir, tmp, target, buf }),
                }
            }
            AtomicPublishState::Opening { .. }
            | AtomicPublishState::Writing { .. }
            | AtomicPublishState::Flushing { .. }
            | AtomicPublishState::ClosingBeforeRename { .. }
            | AtomicPublishState::Renaming { .. }
            | AtomicPublishState::SyncingDir
            | AtomicPublishState::ClosingToCleanup { .. }
            | AtomicPublishState::RemovingTmp { .. } => {
                unreachable!("AtomicPublishMachine::start called on non-Start state")
            }
        }
    }

    fn resume(
        self,
        answer: FsAnswer<'io, 'a, '_>,
        _cx: &mut (),
    ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match (self.0, answer) {
            (
                AtomicPublishState::Opening { dir, tmp, target, buf },
                FsAnswer::OpenedWrite(Ok(file)),
            ) => {
                let req = file.write_from(WriteFromOpts { at: ByteOffset(0), buf });
                Turn::Ask {
                    request: FsRequest::WriteFrom(req),
                    next: Self(AtomicPublishState::Writing { dir, tmp, target, file }),
                }
            }
            (AtomicPublishState::Opening { buf, .. }, FsAnswer::OpenedWrite(Err(err))) => {
                buf.release();
                Turn::Fail(AtomicPublishError::Fs(err))
            }
            (
                AtomicPublishState::Writing { dir, tmp, target, file },
                FsAnswer::Written(Ok(buf)),
            ) => {
                buf.release();
                Turn::Ask {
                    request: FsRequest::Flush(file.flush()),
                    next: Self(AtomicPublishState::Flushing { dir, tmp, target }),
                }
            }
            (AtomicPublishState::Writing { dir, tmp, file, .. }, FsAnswer::Written(Err(be))) => {
                be.buf.release();
                Turn::Ask {
                    request: FsRequest::Close(file.close()),
                    next: Self(AtomicPublishState::ClosingToCleanup {
                        dir,
                        tmp,
                        err: AtomicPublishError::Fs(be.error),
                    }),
                }
            }
            (
                AtomicPublishState::Flushing { dir, tmp, target },
                FsAnswer::Flushed(Ok((file, proof))),
            ) => Turn::Ask {
                request: FsRequest::Close(file.close()),
                next: Self(AtomicPublishState::ClosingBeforeRename { dir, tmp, target, proof }),
            },
            (AtomicPublishState::Flushing { dir, tmp, .. }, FsAnswer::Flushed(Err((file, err)))) => {
                Turn::Ask {
                    request: FsRequest::Close(file.close()),
                    next: Self(AtomicPublishState::ClosingToCleanup {
                        dir,
                        tmp,
                        err: AtomicPublishError::Fs(err),
                    }),
                }
            }
            (
                AtomicPublishState::ClosingBeforeRename { dir, target, proof, .. },
                FsAnswer::Closed(Ok(())),
            ) => {
                let req = dir.rename_flushed(RenameFlushedOpts { flushed: proof, to: target });
                Turn::Ask {
                    request: FsRequest::RenameFlushed(req),
                    next: Self(AtomicPublishState::Renaming { dir }),
                }
            }
            (
                AtomicPublishState::ClosingBeforeRename { dir, tmp, .. },
                FsAnswer::Closed(Err(err)),
            ) => Turn::Ask {
                request: FsRequest::Remove(dir.remove(tmp)),
                next: Self(AtomicPublishState::RemovingTmp { err: AtomicPublishError::Fs(err) }),
            },
            (AtomicPublishState::Renaming { dir }, FsAnswer::Renamed(Ok(()))) => Turn::Ask {
                request: FsRequest::SyncDir(dir.sync_dir()),
                next: Self(AtomicPublishState::SyncingDir),
            },
            (AtomicPublishState::SyncingDir, FsAnswer::SyncedDir(Ok(()))) => Turn::Done(()),
            (AtomicPublishState::Renaming { .. }, FsAnswer::Renamed(Err(err)))
            | (AtomicPublishState::SyncingDir, FsAnswer::SyncedDir(Err(err))) => {
                Turn::Fail(AtomicPublishError::Fs(err))
            }
            (AtomicPublishState::ClosingToCleanup { dir, tmp, err }, FsAnswer::Closed(_)) => {
                Turn::Ask {
                    request: FsRequest::Remove(dir.remove(tmp)),
                    next: Self(AtomicPublishState::RemovingTmp { err }),
                }
            }
            (AtomicPublishState::RemovingTmp { err }, FsAnswer::Removed(_)) => Turn::Fail(err),
            _ => unreachable!("typed FsAnswer always matches active AtomicPublishMachine state"),
        }
    }

    fn on_cancel(
        self,
        stopped: Stopped,
        _cx: &mut (),
    ) -> CancelAction<Turn<Self, Self::Req, Self::Out, Self::Err>> {
        match self.0 {
            AtomicPublishState::Start { buf, .. } | AtomicPublishState::Opening { buf, .. } => {
                buf.release();
                CancelAction::Abandon
            }
            AtomicPublishState::Writing { dir, tmp, file, .. } => {
                CancelAction::Cleanup(Turn::Ask {
                    request: FsRequest::Close(file.close()),
                    next: Self(AtomicPublishState::ClosingToCleanup {
                        dir,
                        tmp,
                        err: AtomicPublishError::Stopped(stopped.reason),
                    }),
                })
            }
            AtomicPublishState::ClosingBeforeRename { dir, tmp, .. } => {
                CancelAction::Cleanup(Turn::Ask {
                    request: FsRequest::Remove(dir.remove(tmp)),
                    next: Self(AtomicPublishState::RemovingTmp {
                        err: AtomicPublishError::Stopped(stopped.reason),
                    }),
                })
            }
            AtomicPublishState::Flushing { .. }
            | AtomicPublishState::Renaming { .. }
            | AtomicPublishState::SyncingDir
            | AtomicPublishState::ClosingToCleanup { .. }
            | AtomicPublishState::RemovingTmp { .. } => CancelAction::Abandon,
        }
    }
}

const S_START: StateId = StateId(0);
const S_OPENING: StateId = StateId(1);
const S_WRITING: StateId = StateId(2);
const S_FLUSHING: StateId = StateId(3);
const S_CLOSING_BEFORE_RENAME: StateId = StateId(4);
const S_RENAMING: StateId = StateId(5);
const S_SYNCING_DIR: StateId = StateId(6);
const S_CLOSING_TO_CLEANUP: StateId = StateId(7);
const S_REMOVING_TMP: StateId = StateId(8);
const H_TMP_FILE: HandleSlot = HandleSlot(0);

const ATOMIC_PUBLISH_STATES: [StateNode; 9] = [
    StateNode { id: S_START, name: "Start", loop_state: false, initial_handles: 0 },
    StateNode { id: S_OPENING, name: "Opening", loop_state: false, initial_handles: 0 },
    StateNode { id: S_WRITING, name: "Writing", loop_state: false, initial_handles: 0 },
    StateNode { id: S_FLUSHING, name: "Flushing", loop_state: false, initial_handles: 0 },
    StateNode {
        id: S_CLOSING_BEFORE_RENAME,
        name: "ClosingBeforeRename",
        loop_state: false,
        initial_handles: 0,
    },
    StateNode { id: S_RENAMING, name: "Renaming", loop_state: false, initial_handles: 0 },
    StateNode { id: S_SYNCING_DIR, name: "SyncingDir", loop_state: false, initial_handles: 0 },
    StateNode {
        id: S_CLOSING_TO_CLEANUP,
        name: "ClosingToCleanup",
        loop_state: false,
        initial_handles: 0,
    },
    StateNode { id: S_REMOVING_TMP, name: "RemovingTmp", loop_state: false, initial_handles: 0 },
];

const ATOMIC_PUBLISH_EDGES: [GraphEdge; 23] = [
    GraphEdge {
        from: S_START,
        kind: EdgeKind::Ask { to: S_OPENING },
        handle_op: HandleOp::None,
        label: "start_open",
    },
    GraphEdge {
        from: S_OPENING,
        kind: EdgeKind::Ask { to: S_WRITING },
        handle_op: HandleOp::Open(H_TMP_FILE),
        label: "open_ok_write",
    },
    GraphEdge {
        from: S_OPENING,
        kind: EdgeKind::Fail,
        handle_op: HandleOp::None,
        label: "open_err",
    },
    GraphEdge {
        from: S_OPENING,
        kind: EdgeKind::Abandon,
        handle_op: HandleOp::None,
        label: "open_cancel_abandon",
    },
    GraphEdge {
        from: S_WRITING,
        kind: EdgeKind::Ask { to: S_FLUSHING },
        handle_op: HandleOp::None,
        label: "write_ok_flush",
    },
    GraphEdge {
        from: S_WRITING,
        kind: EdgeKind::Ask { to: S_CLOSING_TO_CLEANUP },
        handle_op: HandleOp::Close(H_TMP_FILE),
        label: "write_err_close",
    },
    GraphEdge {
        from: S_WRITING,
        kind: EdgeKind::CancelCleanup { to: S_CLOSING_TO_CLEANUP },
        handle_op: HandleOp::Close(H_TMP_FILE),
        label: "write_cancel_close",
    },
    GraphEdge {
        from: S_FLUSHING,
        kind: EdgeKind::Ask { to: S_CLOSING_BEFORE_RENAME },
        handle_op: HandleOp::Close(H_TMP_FILE),
        label: "flush_ok_close",
    },
    GraphEdge {
        from: S_FLUSHING,
        kind: EdgeKind::Ask { to: S_CLOSING_TO_CLEANUP },
        handle_op: HandleOp::Close(H_TMP_FILE),
        label: "flush_err_close",
    },
    GraphEdge {
        from: S_FLUSHING,
        kind: EdgeKind::Abandon,
        handle_op: HandleOp::None,
        label: "flush_cancel_abandon",
    },
    GraphEdge {
        from: S_CLOSING_BEFORE_RENAME,
        kind: EdgeKind::Ask { to: S_RENAMING },
        handle_op: HandleOp::None,
        label: "close_ok_rename",
    },
    GraphEdge {
        from: S_CLOSING_BEFORE_RENAME,
        kind: EdgeKind::Ask { to: S_REMOVING_TMP },
        handle_op: HandleOp::None,
        label: "close_err_remove",
    },
    GraphEdge {
        from: S_CLOSING_BEFORE_RENAME,
        kind: EdgeKind::CancelCleanup { to: S_REMOVING_TMP },
        handle_op: HandleOp::None,
        label: "close_cancel_remove",
    },
    GraphEdge {
        from: S_RENAMING,
        kind: EdgeKind::Ask { to: S_SYNCING_DIR },
        handle_op: HandleOp::None,
        label: "rename_ok_sync_dir",
    },
    GraphEdge {
        from: S_RENAMING,
        kind: EdgeKind::Fail,
        handle_op: HandleOp::None,
        label: "rename_err",
    },
    GraphEdge {
        from: S_RENAMING,
        kind: EdgeKind::Abandon,
        handle_op: HandleOp::None,
        label: "rename_cancel_abandon",
    },
    GraphEdge {
        from: S_SYNCING_DIR,
        kind: EdgeKind::Done,
        handle_op: HandleOp::None,
        label: "sync_dir_ok",
    },
    GraphEdge {
        from: S_SYNCING_DIR,
        kind: EdgeKind::Fail,
        handle_op: HandleOp::None,
        label: "sync_dir_err",
    },
    GraphEdge {
        from: S_SYNCING_DIR,
        kind: EdgeKind::Abandon,
        handle_op: HandleOp::None,
        label: "sync_dir_cancel_abandon",
    },
    GraphEdge {
        from: S_CLOSING_TO_CLEANUP,
        kind: EdgeKind::Ask { to: S_REMOVING_TMP },
        handle_op: HandleOp::None,
        label: "cleanup_close_remove",
    },
    GraphEdge {
        from: S_CLOSING_TO_CLEANUP,
        kind: EdgeKind::Abandon,
        handle_op: HandleOp::None,
        label: "cleanup_close_abandon",
    },
    GraphEdge {
        from: S_REMOVING_TMP,
        kind: EdgeKind::Fail,
        handle_op: HandleOp::None,
        label: "cleanup_remove_fail",
    },
    GraphEdge {
        from: S_REMOVING_TMP,
        kind: EdgeKind::Abandon,
        handle_op: HandleOp::None,
        label: "cleanup_remove_abandon",
    },
];

/// Exact reflected state graph of [`AtomicPublishMachine`], verified by
/// [`blaze_machine::verify_graph`].
pub const fn atomic_publish_graph() -> MachineGraph<'static> {
    MachineGraph {
        name: "AtomicPublish",
        entry: S_START,
        grace_max_steps: 2,
        states: &ATOMIC_PUBLISH_STATES,
        edges: &ATOMIC_PUBLISH_EDGES,
    }
}

/// Options for [`publish_atomic_direct`] (`#[blaze::args]`, rule 83).
#[blaze::args]
pub struct DirectAtomicPublishOpts<'a> {
    pub tmp: FileName,
    pub target: FileName,
    pub bytes: &'a [u8],
    pub task: TaskId = TaskId(1),
}

/// Failure returned by [`publish_atomic_direct`] when driving [`AtomicPublishMachine`] over
/// [`DirectFsHandler`] and [`crate::cx::Cx::drive_machine`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DirectAtomicPublishError {
    /// Driving [`AtomicPublishMachine`] failed (filesystem error, stop, handler error, or step
    /// budget exhaustion).
    Drive(blaze_machine::DriveError<AtomicPublishError, DirectFsHandlerError>),
    /// Normal task exit detected leaked handles in [`DirectFsHandler::finish_task`].
    Leaked(DirectFsHandlerError),
}

impl fmt::Display for DirectAtomicPublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DirectAtomicPublishError::Drive(err) => {
                write!(f, "direct atomic publish failed: {err}")
            }
            DirectAtomicPublishError::Leaked(err) => {
                write!(f, "direct atomic publish leaked handles: {err}")
            }
        }
    }
}

impl core::error::Error for DirectAtomicPublishError {}

/// Thin zero-`Effect`/`Done`-bridge adapter dispatching the [`FsRequest`] steps emitted by
/// [`AtomicPublishMachine`] directly onto [`DirectFsHandler`].
struct DirectAtomicPublishAdapter<'io, 'b, 'h, B: DirectFsBackend> {
    handler: &'h mut DirectFsHandler<'io, 'b, B>,
}

impl<'io, 'a, 'b, 'h, B: DirectFsBackend> blaze_machine::Handler<FsRequest<'io, 'a>>
    for DirectAtomicPublishAdapter<'io, 'b, 'h, B>
{
    type Error = DirectFsHandlerError;

    fn perform<Out>(
        &mut self,
        request: FsRequest<'io, 'a>,
        deliver: impl FnOnce(blaze_machine::Perform<FsAnswer<'io, 'a, '_>>) -> Out,
    ) -> Result<Out, Self::Error> {
        use blaze_machine::{Handler as _, Perform};

        match request {
            FsRequest::OpenWrite(req) => self.handler.perform(req, |perf| {
                deliver(match perf {
                    Perform::Answered(ans) => Perform::Answered(FsAnswer::OpenedWrite(ans)),
                    Perform::Stopped(s) => Perform::Stopped(s),
                })
            }),
            FsRequest::WriteFrom(req) => self.handler.perform(req, |perf| {
                deliver(match perf {
                    Perform::Answered(ans) => Perform::Answered(FsAnswer::Written(ans)),
                    Perform::Stopped(s) => Perform::Stopped(s),
                })
            }),
            FsRequest::Flush(req) => self.handler.perform(req, |perf| {
                deliver(match perf {
                    Perform::Answered(ans) => Perform::Answered(FsAnswer::Flushed(ans)),
                    Perform::Stopped(s) => Perform::Stopped(s),
                })
            }),
            FsRequest::Close(req) => self.handler.perform(req, |perf| {
                deliver(match perf {
                    Perform::Answered(ans) => Perform::Answered(FsAnswer::Closed(ans)),
                    Perform::Stopped(s) => Perform::Stopped(s),
                })
            }),
            FsRequest::RenameFlushed(req) => self.handler.perform(req, |perf| {
                deliver(match perf {
                    Perform::Answered(ans) => Perform::Answered(FsAnswer::Renamed(ans)),
                    Perform::Stopped(s) => Perform::Stopped(s),
                })
            }),
            FsRequest::SyncDir(req) => self.handler.perform(req, |perf| {
                deliver(match perf {
                    Perform::Answered(ans) => Perform::Answered(FsAnswer::SyncedDir(ans)),
                    Perform::Stopped(s) => Perform::Stopped(s),
                })
            }),
            FsRequest::Remove(_req) => Ok(deliver(Perform::Answered(FsAnswer::Removed(Err(
                FsError::Io {
                    path: None,
                    // alloc: error path when a direct backend lacks `remove`.
                    message: alloc::string::String::from(
                        "DirectFsBackend does not implement remove",
                    ),
                },
            ))))),
            _ => unreachable!(
                "AtomicPublishMachine only emits OpenWrite/WriteFrom/Flush/Close/RenameFlushed/SyncDir/Remove"
            ),
        }
    }
}

/// Drives an [`AtomicPublishMachine`] over an existing [`DirectFsHandler`] through
/// [`crate::cx::Cx::drive_machine`] with zero `Effect`/`Done` bridge.
pub fn publish_atomic_machine<'io, 'a, 'b, B: DirectFsBackend>(
    cx: &crate::cx::Cx,
    handler: &mut DirectFsHandler<'io, 'b, B>,
    machine: AtomicPublishMachine<'io, 'a>,
    opts: blaze_machine::DriveOpts,
) -> Result<(), blaze_machine::DriveError<AtomicPublishError, DirectFsHandlerError>> {
    let mut adapter = DirectAtomicPublishAdapter { handler };
    cx.drive_machine("bstd.fs.atomic_publish_direct", &mut adapter, &mut (), machine, opts)
}

/// Durably publishes `opts.bytes` to `opts.target` via `opts.tmp` by driving
/// [`AtomicPublishMachine`] over any [`DirectFsBackend`] ([`NativeDirectFs`],
/// [`blaze_core::sim::Sim`], or [`InMemoryDirectFs`]) through [`DirectFsHandler`] and
/// [`crate::cx::Cx::drive_machine`] with zero `Effect`/`Done` bridge, verifying zero leaked
/// handles on normal exit.
pub fn publish_atomic_direct<B: DirectFsBackend>(
    cx: &crate::cx::Cx,
    backend: &mut B,
    opts: DirectAtomicPublishOpts<'_>,
) -> Result<(), DirectAtomicPublishError> {
    let alloc = blaze_own::Alloc::global();
    let DirectAtomicPublishOpts { tmp, target, bytes, task } = opts;
    blaze_own::with_brand(|brand| {
        let mut handler = DirectFsHandler::new(backend, task, brand, alloc);
        let dir = handler.root();
        let buf = DmaBuf::from_slice_in(bytes, alloc);
        let machine = AtomicPublishMachine::new(AtomicPublishOpts { dir, tmp, target, buf });
        let driven =
            publish_atomic_machine(cx, &mut handler, machine, blaze_machine::DriveOpts { max_steps: 16 });
        let exit = if driven.is_ok() { ExitKind::Normal } else { ExitKind::Cancelled };
        let finish = handler.finish_task(exit);
        match driven {
            Ok(()) => finish.map(|_| ()).map_err(DirectAtomicPublishError::Leaked),
            Err(err) => Err(DirectAtomicPublishError::Drive(err)),
        }
    })
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use blaze_core::{
        FileName,
        handle::{ExitKind, TaskId},
        request::SimHandler,
        sim::{Faults, Sim},
    };
    use blaze_machine::{DriveOpts, emit_lean, verify_graph};
    use blaze_own::{FixedString, testing::heap, with_brand};

    use super::{AtomicPublishMachine, AtomicPublishOpts, DmaBuf, atomic_publish_graph};
    use crate::{
        io::{Io, Limits, SimWorld},
        mem::TASK_FRAME,
    };

    #[test]
    fn atomic_publish_machine_graph_is_verified_and_emits_lean() {
        let graph = atomic_publish_graph();
        assert_eq!(verify_graph(&graph), Ok(()));

        let mut lean = FixedString::<8192>::new();
        assert_eq!(emit_lean(&graph, &mut lean), Ok(()));
        let text = lean.text().expect("Lean output fits in 8192 bytes");
        assert!(text.contains("namespace AtomicPublish"), "missing namespace:\n{text}");
        assert!(text.contains("inductive State where"), "missing State:\n{text}");
        assert!(
            text.contains("theorem verified_machine_no_leak"),
            "missing verified_machine_no_leak:\n{text}"
        );
        assert!(text.contains("end AtomicPublish"), "missing end namespace:\n{text}");
    }

    #[test]
    fn atomic_publish_machine_runs_over_cx_drive_machine_and_sim_handler() {
        let h = heap();
        let io = Io::simulated(SimWorld::new(42, Limits::USUAL));
        let cx = io.cx(Duration::from_secs(10), TASK_FRAME).expect("root cx");
        let mut sim = Sim::new(42, Faults::default());

        let tmp = FileName::new("manifest.tmp").expect("valid tmp name");
        let target = FileName::new("manifest.bin").expect("valid target name");
        let payload = b"blaze-sans-io-atomic-publish-payload";

        with_brand(|brand| {
            let mut handler = SimHandler::new(&mut sim, TaskId(1), brand, h.alloc());
            let dir = handler.root();
            let buf = DmaBuf::from_slice_in(payload, h.alloc());
            let machine = AtomicPublishMachine::new(AtomicPublishOpts {
                dir,
                tmp: tmp.clone(),
                target: target.clone(),
                buf,
            });

            let res = cx.drive_machine(
                "fs.atomic_publish",
                &mut handler,
                &mut (),
                machine,
                DriveOpts { max_steps: 16 },
            );
            assert_eq!(res, Ok(()));
            assert_eq!(handler.finish_task(ExitKind::Normal), Ok(0));
        });

        // Durable publish survives a simulated crash intact and leaves no leftover temp file.
        sim.crash();
        assert!(sim.exists(&target), "published target must survive crash");
        assert_eq!(sim.contents(&target).as_slice(), payload);
        assert!(!sim.exists(&tmp), "temporary file must not remain after rename");
    }

    #[test]
    fn bstd_fs_atomic_publish_machine_over_native_direct_fs() {
        use blaze_core::{
            request::{DirectFsHandler, NativeDirectFs, ReadIntoOpts},
            temp::TempDir,
        };
        use blaze_machine::{Handler, Perform};

        use super::{
            ByteLen, ByteOffset, DirectAtomicPublishOpts, DirectFsBackend, publish_atomic_direct,
            publish_atomic_machine,
        };

        fn publish_and_readback<B: DirectFsBackend>(
            cx: &crate::cx::Cx,
            backend: &mut B,
            tmp: &FileName,
            target: &FileName,
            payload: &[u8],
            alloc: blaze_own::Alloc<'static>,
        ) -> alloc::vec::Vec<u8> {
            let mut out = alloc::vec::Vec::new();
            with_brand(|brand| {
                let mut handler = DirectFsHandler::new(backend, TaskId(21), brand, alloc);
                let root = handler.root();
                let buf = DmaBuf::from_slice_in(payload, alloc);
                let machine = AtomicPublishMachine::new(AtomicPublishOpts {
                    dir: root,
                    tmp: tmp.clone(),
                    target: target.clone(),
                    buf,
                });
                assert_eq!(
                    publish_atomic_machine(cx, &mut handler, machine, DriveOpts { max_steps: 16 }),
                    Ok(())
                );

                let ro = root.narrow_read_only();
                let file = handler
                    .perform(ro.open_read(target.clone()), |p| match p {
                        Perform::Answered(res) => res,
                        _ => unreachable!(),
                    })
                    .expect("open_read handler ok")
                    .expect("open_read ok");
                let filled = handler
                    .perform(
                        file.read_into(ReadIntoOpts {
                            at: ByteOffset(0),
                            buf: DmaBuf::with_capacity_in(
                                ByteLen((payload.len() + 32) as u32),
                                alloc,
                            ),
                        }),
                        |p| match p {
                            Perform::Answered(res) => res,
                            _ => unreachable!(),
                        },
                    )
                    .expect("read_into handler ok")
                    .expect("read_into ok");
                assert_eq!(filled.filled, ByteLen(payload.len() as u32));
                out.extend_from_slice(filled.buf.as_slice());
                filled.buf.release();
                assert_eq!(
                    handler
                        .perform(file.close(), |p| match p {
                            Perform::Answered(res) => res,
                            _ => unreachable!(),
                        })
                        .expect("close handler ok"),
                    Ok(())
                );
                assert_eq!(handler.finish_checked(), Ok(()));
            });
            out
        }

        let h = heap();
        let io = Io::simulated(SimWorld::new(88, Limits::USUAL));
        let cx = io.cx(Duration::from_secs(10), TASK_FRAME).expect("root cx");

        let dir = TempDir::for_test("bstd-atomic-publish-direct-");
        let root_path = dir.path().to_path_buf();
        let mut native = NativeDirectFs::new(root_path.clone());
        let mut sim = Sim::new(88, Faults::default());

        let tmp = FileName::new("record.tmp").expect("valid tmp name");
        let target = FileName::new("record.bin").expect("valid target name");
        let payload: alloc::vec::Vec<u8> =
            (0..2048usize).map(|i| ((i.wrapping_mul(29).wrapping_add(11)) & 0xFF) as u8).collect();

        // 1. Publish durably via `publish_atomic_direct` over `NativeDirectFs` AND `Sim`
        //    with zero `Effect`/`Done` bridge and zero leaked handles.
        publish_atomic_direct(
            &cx,
            &mut native,
            DirectAtomicPublishOpts {
                tmp: tmp.clone(),
                target: target.clone(),
                bytes: &payload,
                task: TaskId(11),
            },
        )
        .expect("native publish_atomic_direct succeeds");

        publish_atomic_direct(
            &cx,
            &mut sim,
            DirectAtomicPublishOpts {
                tmp: tmp.clone(),
                target: target.clone(),
                bytes: &payload,
                task: TaskId(12),
            },
        )
        .expect("sim publish_atomic_direct succeeds");

        // Simulated crash keeps the durable publish intact and leaves no tmp file behind.
        sim.crash();
        assert!(sim.exists(&target), "sim target must survive crash");
        assert!(!sim.exists(&tmp), "sim tmp must not remain after rename");
        assert_eq!(sim.contents(&target).as_slice(), payload.as_slice());

        // Real OS temp dir holds the exact published bytes and no leftover tmp file.
        let disk_bytes = std::fs::read(dir.join("record.bin")).expect("read native published file");
        assert_eq!(disk_bytes.as_slice(), payload.as_slice());
        assert!(!dir.join("record.tmp").exists(), "native tmp must not remain after rename");

        // 2. Drive `AtomicPublishMachine` via `publish_atomic_machine` over caller-owned
        //    `DirectFsHandler` + direct `ReadInto`, verifying byte-for-byte parity & zero leaks.
        let tmp2 = FileName::new("record2.tmp").expect("valid tmp2 name");
        let target2 = FileName::new("record2.bin").expect("valid target2 name");
        let payload2 = b"blaze-direct-atomic-publish-machine-v2";

        let native_readback =
            publish_and_readback(&cx, &mut native, &tmp2, &target2, payload2, h.alloc());
        let sim_readback =
            publish_and_readback(&cx, &mut sim, &tmp2, &target2, payload2, h.alloc());
        assert_eq!(native_readback.as_slice(), payload2);
        assert_eq!(native_readback, sim_readback);

        drop(dir);
        assert!(!root_path.exists(), "temp dir guard must clean up {}", root_path.display());
    }
}
