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
    Dir, File, FileId, FixedFileSlot, Flushed, Lock, ReadOnly, ReadWrite, Unflushed,
};
pub use blaze_core::request::{
    BufError, ByteLen, ByteOffset, Close, CopyData, CopyDataOpts, DmaBuf, FilledBuf, FixedBufIndex,
    Flush, FlushedProof, FsAnswer, FsError, FsRequest, LENT_READ_MAX, List, ListOpts,
    Lock as LockReq, LockOpts, LockOutcome, MakeDir, MaxBytes, OpenOpts, OpenRead, OpenWrite, Read,
    ReadAll, ReadAllOpts, ReadInto, ReadIntoOpts, ReadOpts, Remove, RemoveDir, RenameFlushed,
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
}
