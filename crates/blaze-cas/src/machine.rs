//! Explicit typestate sans-IO machine for publishing a content-addressed record to a CAS
//! segment/object file durably over [`blaze_core::request::FsRequest`]
//! (`docs/v2/plan/design/sans-io-machines.md`).
//!
//! Durability sequence:
//! `OpenWrite(tmp)` -> `WriteFrom(tmp, record_buf)` -> `Flush(tmp)` -> `Close(tmp)` ->
//! `RenameFlushed(tmp -> final_name)` -> `SyncDir` -> `Turn::Done(CasPublished { key, bytes })`.
//!
//! Invariants:
//! - Typestate durability: `RenameFlushed` requires a [`FlushedProof`] minted only by [`Flush`],
//!   so skipping `fsync` before atomic rename is a compile error outside the explicit planted seam.
//! - Zero leaked handles: on ANY error while holding an open linear [`File`], the machine
//!   transitions through `ClosingOnError` to close the handle before `Turn::Fail`.
//! - Bounded cancellation cleanup: `on_cancel` returns [`CancelAction::Cleanup`] closing any open
//!   [`File`] and removing the temporary file within [`blaze_machine::GraceBudget`].

#![blaze::sans_io]
#![forbid(unsafe_code)]

use core::fmt;

use blaze_core::FileName;
use blaze_core::handle::{Dir, File, ReadWrite, Unflushed};
use blaze_core::request::{
    ByteOffset, DmaBuf, FlushedProof, FsAnswer, FsError, FsRequest, OpenOpts, RenameFlushedOpts,
    RenameUnsyncedOpts, WriteFromOpts,
};
use blaze_machine::{
    CancelAction, CancelReason, EdgeKind, GraphEdge, HandleOp, HandleSlot, Machine, MachineGraph,
    StateId, StateNode, Stopped, Turn,
};
use blaze_own::planted::{NoPlant, Planted, Plants};

use crate::Key;

/// Planted-bug switches for [`CasRecordPublishMachine`] DST controls (rules 43 & 53).
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum CasPublishPlant {
    #[default]
    None,
    /// Planted bug: skip `Flush` and `SyncDir`, closing the unflushed temp file and renaming via
    /// `RenameUnsynced` so a crash can lose or tear the published record.
    SkipFlushBeforeRename,
    /// Planted bug: return `Turn::Fail` directly on `WriteFrom` or `Flush` failure without
    /// closing the open linear `File` handle (caught by `DriverHandler::finish_task`).
    SkipCloseOnError,
}

impl Plants for CasPublishPlant {}

/// Options for constructing [`CasRecordPublishMachine`] (`#[blaze::args]`, rule 83).
#[blaze::args]
pub struct CasPublishOpts<'io, 'a> {
    /// Directory scope holding both `tmp` and `final_name`.
    pub dir: Dir<'io, ReadWrite>,
    /// Content key of the CAS record being published.
    pub key: Key,
    /// Temporary file name written and flushed before atomic rename.
    pub tmp: FileName,
    /// Final immutable CAS segment/object file name.
    pub final_name: FileName,
    /// Caller-owned DMA buffer containing the full record bytes.
    pub record_buf: DmaBuf<'a>,
}

/// Terminal success outcome of [`CasRecordPublishMachine`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CasPublished {
    pub key: Key,
    pub bytes: u64,
}

/// Terminal failure outcome of [`CasRecordPublishMachine`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CasPublishError {
    Fs(FsError),
    Cancelled(CancelReason),
}

impl fmt::Display for CasPublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CasPublishError::Fs(err) => write!(f, "CAS record publish failed: {err}"),
            CasPublishError::Cancelled(reason) => {
                write!(f, "CAS record publish cancelled ({reason:?})")
            }
        }
    }
}

enum State<'io, 'a> {
    Init {
        tmp: FileName,
        final_name: FileName,
        record_buf: DmaBuf<'a>,
    },
    Opening {
        tmp: FileName,
        final_name: FileName,
        record_buf: DmaBuf<'a>,
    },
    Writing {
        tmp: FileName,
        final_name: FileName,
        file: File<'io, Unflushed>,
    },
    Flushing {
        final_name: FileName,
    },
    ClosingBeforeRename {
        tmp: FileName,
        final_name: FileName,
        proof: Option<FlushedProof>,
        unsynced_from: Option<FileName>,
    },
    ClosingOnError {
        err: FsError,
    },
    Renaming {
        sync_dir: bool,
    },
    SyncingDir,
    ClosingOnCancel {
        tmp: FileName,
        reason: CancelReason,
    },
    RemovingOnCancel {
        reason: CancelReason,
    },
}

/// Explicit typestate sans-IO machine publishing one content-addressed CAS record durably over
/// [`FsRequest`].
pub struct CasRecordPublishMachine<'io, 'a, P: Planted<CasPublishPlant> = NoPlant> {
    dir: Dir<'io, ReadWrite>,
    key: Key,
    bytes: u64,
    state: State<'io, 'a>,
    plant: P,
}

impl<'io, 'a> CasRecordPublishMachine<'io, 'a, NoPlant> {
    /// Creates a production [`CasRecordPublishMachine`] with zero planted bugs.
    #[inline]
    pub fn new(opts: CasPublishOpts<'io, 'a>) -> Self {
        let bytes = opts.record_buf.len() as u64;
        Self {
            dir: opts.dir,
            key: opts.key,
            bytes,
            state: State::Init {
                tmp: opts.tmp,
                final_name: opts.final_name,
                record_buf: opts.record_buf,
            },
            plant: NoPlant,
        }
    }
}

impl<'io, 'a, P: Planted<CasPublishPlant>> CasRecordPublishMachine<'io, 'a, P> {
    /// Creates a [`CasRecordPublishMachine`] carrying a planted-bug seam for DST controls.
    #[inline]
    pub fn with_plant(opts: CasPublishOpts<'io, 'a>, plant: P) -> Self {
        let bytes = opts.record_buf.len() as u64;
        Self {
            dir: opts.dir,
            key: opts.key,
            bytes,
            state: State::Init {
                tmp: opts.tmp,
                final_name: opts.final_name,
                record_buf: opts.record_buf,
            },
            plant,
        }
    }
}

impl<'io, 'a, P: Planted<CasPublishPlant>> Machine<()> for CasRecordPublishMachine<'io, 'a, P> {
    type Req = FsRequest<'io, 'a>;
    type Out = CasPublished;
    type Err = CasPublishError;

    fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        let Self { dir, key, bytes, state, plant } = self;
        match state {
            State::Init { tmp, final_name, record_buf } => {
                let req = dir.open_write(OpenOpts {
                    path: tmp.clone(),
                    create: true,
                    truncate: true,
                });
                Turn::Ask {
                    request: FsRequest::OpenWrite(req),
                    next: Self {
                        dir,
                        key,
                        bytes,
                        state: State::Opening { tmp, final_name, record_buf },
                        plant,
                    },
                }
            }
            State::Opening { .. }
            | State::Writing { .. }
            | State::Flushing { .. }
            | State::ClosingBeforeRename { .. }
            | State::ClosingOnError { .. }
            | State::Renaming { .. }
            | State::SyncingDir
            | State::ClosingOnCancel { .. }
            | State::RemovingOnCancel { .. } => {
                unreachable!("CasRecordPublishMachine::start called on non-Init state")
            }
        }
    }

    fn resume(
        self,
        answer: FsAnswer<'io, 'a, '_>,
        _cx: &mut (),
    ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        let Self { dir, key, bytes, state, plant } = self;
        match (state, answer) {
            (
                State::Opening { tmp, final_name, record_buf },
                FsAnswer::OpenedWrite(Ok(file)),
            ) => {
                let req = file.write_from(WriteFromOpts { at: ByteOffset(0), buf: record_buf });
                Turn::Ask {
                    request: FsRequest::WriteFrom(req),
                    next: Self {
                        dir,
                        key,
                        bytes,
                        state: State::Writing { tmp, final_name, file },
                        plant,
                    },
                }
            }
            (State::Opening { record_buf, .. }, FsAnswer::OpenedWrite(Err(err))) => {
                record_buf.release();
                Turn::Fail(CasPublishError::Fs(err))
            }
            (State::Writing { tmp, final_name, file }, FsAnswer::Written(Ok(buf))) => {
                buf.release();
                if plant.has(CasPublishPlant::SkipFlushBeforeRename) {
                    Turn::Ask {
                        request: FsRequest::Close(file.close()),
                        next: Self {
                            dir,
                            key,
                            bytes,
                            state: State::ClosingBeforeRename {
                                tmp: tmp.clone(),
                                final_name,
                                proof: None,
                                unsynced_from: Some(tmp),
                            },
                            plant,
                        },
                    }
                } else {
                    Turn::Ask {
                        request: FsRequest::Flush(file.flush()),
                        next: Self {
                            dir,
                            key,
                            bytes,
                            state: State::Flushing { final_name },
                            plant,
                        },
                    }
                }
            }
            (State::Writing { file, .. }, FsAnswer::Written(Err(buf_err))) => {
                buf_err.buf.release();
                if plant.has(CasPublishPlant::SkipCloseOnError) {
                    drop(file);
                    Turn::Fail(CasPublishError::Fs(buf_err.error))
                } else {
                    Turn::Ask {
                        request: FsRequest::Close(file.close()),
                        next: Self {
                            dir,
                            key,
                            bytes,
                            state: State::ClosingOnError { err: buf_err.error },
                            plant,
                        },
                    }
                }
            }
            (State::Flushing { final_name }, FsAnswer::Flushed(Ok((file, proof)))) => {
                let tmp = proof.path().clone();
                Turn::Ask {
                    request: FsRequest::Close(file.close()),
                    next: Self {
                        dir,
                        key,
                        bytes,
                        state: State::ClosingBeforeRename {
                            tmp,
                            final_name,
                            proof: Some(proof),
                            unsynced_from: None,
                        },
                        plant,
                    },
                }
            }
            (State::Flushing { .. }, FsAnswer::Flushed(Err((file, err)))) => {
                if plant.has(CasPublishPlant::SkipCloseOnError) {
                    drop(file);
                    Turn::Fail(CasPublishError::Fs(err))
                } else {
                    Turn::Ask {
                        request: FsRequest::Close(file.close()),
                        next: Self {
                            dir,
                            key,
                            bytes,
                            state: State::ClosingOnError { err },
                            plant,
                        },
                    }
                }
            }
            (
                State::ClosingBeforeRename { final_name, proof, unsynced_from, .. },
                FsAnswer::Closed(Ok(())),
            ) => {
                if let Some(flushed) = proof {
                    let req = dir.rename_flushed(RenameFlushedOpts { flushed, to: final_name });
                    Turn::Ask {
                        request: FsRequest::RenameFlushed(req),
                        next: Self {
                            dir,
                            key,
                            bytes,
                            state: State::Renaming { sync_dir: true },
                            plant,
                        },
                    }
                } else {
                    let from = unsynced_from
                        .expect("planted SkipFlushBeforeRename always carries unsynced_from");
                    let req = dir.rename_unsynced(RenameUnsyncedOpts { from, to: final_name });
                    Turn::Ask {
                        request: FsRequest::RenameUnsynced(req),
                        next: Self {
                            dir,
                            key,
                            bytes,
                            state: State::Renaming { sync_dir: false },
                            plant,
                        },
                    }
                }
            }
            (State::ClosingBeforeRename { .. }, FsAnswer::Closed(Err(err)))
            | (State::Renaming { .. }, FsAnswer::Renamed(Err(err)))
            | (State::SyncingDir, FsAnswer::SyncedDir(Err(err))) => {
                Turn::Fail(CasPublishError::Fs(err))
            }
            (State::ClosingOnError { err }, FsAnswer::Closed(_)) => {
                Turn::Fail(CasPublishError::Fs(err))
            }
            (State::Renaming { sync_dir }, FsAnswer::Renamed(Ok(()))) => {
                if sync_dir {
                    Turn::Ask {
                        request: FsRequest::SyncDir(dir.sync_dir()),
                        next: Self { dir, key, bytes, state: State::SyncingDir, plant },
                    }
                } else {
                    Turn::Done(CasPublished { key, bytes })
                }
            }
            (State::SyncingDir, FsAnswer::SyncedDir(Ok(()))) => {
                Turn::Done(CasPublished { key, bytes })
            }
            (State::ClosingOnCancel { tmp, reason }, FsAnswer::Closed(_)) => Turn::Ask {
                request: FsRequest::Remove(dir.remove(tmp)),
                next: Self { dir, key, bytes, state: State::RemovingOnCancel { reason }, plant },
            },
            (State::RemovingOnCancel { reason }, FsAnswer::Removed(_)) => {
                Turn::Fail(CasPublishError::Cancelled(reason))
            }
            _ => unreachable!("FsAnswer always matches active waiting state"),
        }
    }

    fn on_cancel(
        self,
        stopped: Stopped,
        _cx: &mut (),
    ) -> CancelAction<Turn<Self, Self::Req, Self::Out, Self::Err>> {
        let Self { dir, key, bytes, state, plant } = self;
        match state {
            State::Opening { tmp, record_buf, .. } => {
                record_buf.release();
                CancelAction::Cleanup(Turn::Ask {
                    request: FsRequest::Remove(dir.remove(tmp)),
                    next: Self {
                        dir,
                        key,
                        bytes,
                        state: State::RemovingOnCancel { reason: stopped.reason },
                        plant,
                    },
                })
            }
            State::Writing { tmp, file, .. } => CancelAction::Cleanup(Turn::Ask {
                request: FsRequest::Close(file.close()),
                next: Self {
                    dir,
                    key,
                    bytes,
                    state: State::ClosingOnCancel { tmp, reason: stopped.reason },
                    plant,
                },
            }),
            State::ClosingBeforeRename { tmp, .. } => CancelAction::Cleanup(Turn::Ask {
                request: FsRequest::Remove(dir.remove(tmp)),
                next: Self {
                    dir,
                    key,
                    bytes,
                    state: State::RemovingOnCancel { reason: stopped.reason },
                    plant,
                },
            }),
            State::Init { record_buf, .. } => {
                record_buf.release();
                CancelAction::Abandon
            }
            State::Flushing { .. }
            | State::ClosingOnError { .. }
            | State::Renaming { .. }
            | State::SyncingDir
            | State::ClosingOnCancel { .. }
            | State::RemovingOnCancel { .. } => CancelAction::Abandon,
        }
    }
}

/// Reflected state graph of [`CasRecordPublishMachine`], verified by
/// [`blaze_machine::verify_graph`].
pub fn cas_publish_graph() -> MachineGraph<'static> {
    const INIT: StateId = StateId(0);
    const OPENING: StateId = StateId(1);
    const WRITING: StateId = StateId(2);
    const FLUSHING: StateId = StateId(3);
    const CLOSING_BEFORE_RENAME: StateId = StateId(4);
    const CLOSING_ON_ERROR: StateId = StateId(5);
    const RENAMING: StateId = StateId(6);
    const SYNCING_DIR: StateId = StateId(7);
    const CLOSING_ON_CANCEL: StateId = StateId(8);
    const REMOVING_ON_CANCEL: StateId = StateId(9);
    const TMP_FILE: HandleSlot = HandleSlot(0);

    static STATES: [StateNode; 10] = [
        StateNode { id: INIT, name: "Init", loop_state: false, initial_handles: 0 },
        StateNode { id: OPENING, name: "Opening", loop_state: false, initial_handles: 0 },
        StateNode { id: WRITING, name: "Writing", loop_state: false, initial_handles: 0 },
        StateNode { id: FLUSHING, name: "Flushing", loop_state: false, initial_handles: 0 },
        StateNode {
            id: CLOSING_BEFORE_RENAME,
            name: "ClosingBeforeRename",
            loop_state: false,
            initial_handles: 0,
        },
        StateNode {
            id: CLOSING_ON_ERROR,
            name: "ClosingOnError",
            loop_state: false,
            initial_handles: 0,
        },
        StateNode { id: RENAMING, name: "Renaming", loop_state: false, initial_handles: 0 },
        StateNode { id: SYNCING_DIR, name: "SyncingDir", loop_state: false, initial_handles: 0 },
        StateNode {
            id: CLOSING_ON_CANCEL,
            name: "ClosingOnCancel",
            loop_state: false,
            initial_handles: 0,
        },
        StateNode {
            id: REMOVING_ON_CANCEL,
            name: "RemovingOnCancel",
            loop_state: false,
            initial_handles: 0,
        },
    ];

    static EDGES: [GraphEdge; 25] = [
        GraphEdge {
            from: INIT,
            kind: EdgeKind::Ask { to: OPENING },
            handle_op: HandleOp::None,
            label: "open_tmp",
        },
        GraphEdge {
            from: OPENING,
            kind: EdgeKind::Ask { to: WRITING },
            handle_op: HandleOp::Open(TMP_FILE),
            label: "open_ok_write",
        },
        GraphEdge {
            from: OPENING,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "open_err",
        },
        GraphEdge {
            from: OPENING,
            kind: EdgeKind::CancelCleanup { to: REMOVING_ON_CANCEL },
            handle_op: HandleOp::None,
            label: "open_cancel_remove",
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
            handle_op: HandleOp::Close(TMP_FILE),
            label: "write_ok_planted_close",
        },
        GraphEdge {
            from: WRITING,
            kind: EdgeKind::Ask { to: CLOSING_ON_ERROR },
            handle_op: HandleOp::Close(TMP_FILE),
            label: "write_err_close",
        },
        GraphEdge {
            from: WRITING,
            kind: EdgeKind::CancelCleanup { to: CLOSING_ON_CANCEL },
            handle_op: HandleOp::Close(TMP_FILE),
            label: "write_cancel_close",
        },
        GraphEdge {
            from: FLUSHING,
            kind: EdgeKind::Ask { to: CLOSING_BEFORE_RENAME },
            handle_op: HandleOp::Close(TMP_FILE),
            label: "flush_ok_close",
        },
        GraphEdge {
            from: FLUSHING,
            kind: EdgeKind::Ask { to: CLOSING_ON_ERROR },
            handle_op: HandleOp::Close(TMP_FILE),
            label: "flush_err_close",
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
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "close_err",
        },
        GraphEdge {
            from: CLOSING_BEFORE_RENAME,
            kind: EdgeKind::CancelCleanup { to: REMOVING_ON_CANCEL },
            handle_op: HandleOp::None,
            label: "close_cancel_remove",
        },
        GraphEdge {
            from: CLOSING_ON_ERROR,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "close_on_error_fail",
        },
        GraphEdge {
            from: CLOSING_ON_ERROR,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "close_on_error_abandon",
        },
        GraphEdge {
            from: RENAMING,
            kind: EdgeKind::Ask { to: SYNCING_DIR },
            handle_op: HandleOp::None,
            label: "rename_ok_sync_dir",
        },
        GraphEdge {
            from: RENAMING,
            kind: EdgeKind::Done,
            handle_op: HandleOp::None,
            label: "rename_ok_planted_done",
        },
        GraphEdge {
            from: RENAMING,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "rename_err",
        },
        GraphEdge {
            from: RENAMING,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "rename_cancel_abandon",
        },
        GraphEdge {
            from: SYNCING_DIR,
            kind: EdgeKind::Done,
            handle_op: HandleOp::None,
            label: "sync_dir_ok",
        },
        GraphEdge {
            from: SYNCING_DIR,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "sync_dir_err",
        },
        GraphEdge {
            from: SYNCING_DIR,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "sync_dir_cancel_abandon",
        },
        GraphEdge {
            from: CLOSING_ON_CANCEL,
            kind: EdgeKind::Ask { to: REMOVING_ON_CANCEL },
            handle_op: HandleOp::None,
            label: "cancel_close_then_remove",
        },
        GraphEdge {
            from: REMOVING_ON_CANCEL,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "cancel_remove_done",
        },
    ];

    MachineGraph {
        name: "CasRecordPublish",
        entry: INIT,
        grace_max_steps: 2,
        states: &STATES,
        edges: &EDGES,
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_core::FileName;
    use blaze_core::handle::{ExitKind, TaskId};
    use blaze_core::request::{DmaBuf, DriverHandlerError, FsError, SimHandler};
    use blaze_core::sim::{Faults, Sim};
    use blaze_ids::Hasher;
    use blaze_machine::{DriveError, DriveOpts, drive, emit_lean, verify_graph};
    use blaze_own::{Erased, FixedString, with_brand};

    use super::{
        CasPublishError, CasPublishOpts, CasPublishPlant, CasPublished, CasRecordPublishMachine,
        cas_publish_graph,
    };
    use crate::Key;

    #[test]
    fn cas_publish_graph_verifies_and_emits_lean() {
        let graph = cas_publish_graph();
        assert_eq!(verify_graph(&graph), Ok(()));

        let mut lean = FixedString::<8192>::new();
        assert_eq!(emit_lean(&graph, &mut lean), Ok(()));
        let text = lean.text().expect("Lean output fits in 8192 bytes");
        assert!(text.contains("namespace CasRecordPublish"), "missing namespace:\n{text}");
        assert!(text.contains("inductive State where"), "missing State:\n{text}");
        assert!(
            text.contains("inductive Step : State -> Nat -> State -> Nat -> Prop where"),
            "missing Step:\n{text}"
        );
        assert!(text.contains("def legalTerminal"), "missing legalTerminal:\n{text}");
        assert!(
            text.contains("theorem verified_machine_no_leak"),
            "missing verified_machine_no_leak:\n{text}"
        );
        assert!(text.contains("end CasRecordPublish"), "missing end namespace:\n{text}");
    }

    #[test]
    fn cas_publish_machine_commits_durably_over_sim_handler_with_zero_leaks() {
        let alloc = blaze_own::Alloc::global();

        // Multi-sector payload (2048 bytes = 4 sectors) so an unflushed crash tears visibly.
        let mut payload = std::vec::Vec::with_capacity(2048);
        for i in 0..2048usize {
            payload.push(((i.wrapping_mul(37)) ^ 0xA5) as u8);
        }
        let key = Key(Hasher::new("cas publish test").update(&payload).finish());
        let tmp = FileName::new("cas/seg-0001.tmp").expect("valid tmp name");
        let final_name = FileName::new("cas/seg-0001.obj").expect("valid final name");

        let mut sim = Sim::new(42, Faults::default());
        with_brand(|brand| {
            let mut handler = SimHandler::new(&mut sim, TaskId(1), brand, alloc);
            let root = handler.root();
            let record_buf = DmaBuf::from_slice_in(&payload, alloc);
            let machine = CasRecordPublishMachine::new(CasPublishOpts {
                dir: root,
                key,
                tmp: tmp.clone(),
                final_name: final_name.clone(),
                record_buf,
            });
            let outcome = drive(&mut handler, &mut (), machine, DriveOpts { max_steps: 16 });
            assert_eq!(outcome, Ok(CasPublished { key, bytes: payload.len() as u64 }));
            assert_eq!(handler.finish_task(ExitKind::Normal), Ok(0));
        });

        // Power cut after commit: exact record bytes MUST survive crash intact.
        sim.crash();
        assert!(sim.exists(&final_name), "published CAS record missing after crash");
        assert_eq!(sim.contents(&final_name), payload);
        assert!(!sim.exists(&tmp), "temp file should no longer exist after rename");

        // Planted-bug control: SkipFlushBeforeRename loses or tears durability across crash seeds.
        let mut caught_durability_loss = false;
        for seed in 0u64..32 {
            let mut planted_sim = Sim::new(seed, Faults::default());
            with_brand(|brand| {
                let mut handler = SimHandler::new(&mut planted_sim, TaskId(2), brand, alloc);
                let root = handler.root();
                let record_buf = DmaBuf::from_slice_in(&payload, alloc);
                let machine = CasRecordPublishMachine::with_plant(
                    CasPublishOpts {
                        dir: root,
                        key,
                        tmp: tmp.clone(),
                        final_name: final_name.clone(),
                        record_buf,
                    },
                    CasPublishPlant::SkipFlushBeforeRename,
                );
                let outcome = drive(&mut handler, &mut (), machine, DriveOpts { max_steps: 16 });
                assert_eq!(outcome, Ok(CasPublished { key, bytes: payload.len() as u64 }));
                assert_eq!(handler.finish_task(ExitKind::Normal), Ok(0));
            });
            planted_sim.crash();
            if !planted_sim.exists(&final_name) || planted_sim.contents(&final_name) != payload {
                caught_durability_loss = true;
                break;
            }
        }
        assert!(
            caught_durability_loss,
            "SkipFlushBeforeRename planted control must lose or tear record on Sim::crash"
        );
    }

    #[test]
    fn cas_publish_machine_closes_handle_on_fault_and_planted_control_triggers_leak_error() {
        let alloc = blaze_own::Alloc::global();

        let payload = [0x5Au8; 1024];
        let key = Key(Hasher::new("cas fault test").update(&payload).finish());
        let dir_name = FileName::new("cas").expect("valid dir name");
        let tmp = FileName::new("cas/seg-fault.tmp").expect("valid tmp name");
        let final_name = FileName::new("cas/seg-fault.obj").expect("valid final name");

        // 1. Normal machine: `Sim::fill(cas, 0)` lets `OpenWrite` succeed (Truncate) and faults
        //    `WriteFrom` with `NoSpace`. The machine closes its linear `File` via `ClosingOnError`
        //    before failing, so `finish_task(ExitKind::Normal)` reports zero leaked handles.
        let mut sim = Sim::new(7, Faults::default());
        sim.fill(dir_name.clone(), 0);
        with_brand(|brand| {
            let mut handler = SimHandler::new(&mut sim, TaskId(10), brand, alloc);
            let root = handler.root();
            let record_buf = DmaBuf::from_slice_in(&payload, alloc);
            let machine = CasRecordPublishMachine::with_plant(
                CasPublishOpts {
                    dir: root,
                    key,
                    tmp: tmp.clone(),
                    final_name: final_name.clone(),
                    record_buf,
                },
                CasPublishPlant::None,
            );
            let res = drive(&mut handler, &mut (), machine, DriveOpts { max_steps: 16 });
            assert!(
                matches!(res, Err(DriveError::Machine(CasPublishError::Fs(FsError::NoSpace { .. })))),
                "expected NoSpace fault on WriteFrom, got {res:?}"
            );
            assert_eq!(handler.finish_task(ExitKind::Normal), Ok(0));
        });

        // 2. Planted-bug control: `CasPublishPlant::SkipCloseOnError` drops the open handle
        //    without emitting `Close`, so `finish_task(ExitKind::Normal)` catches the leak!
        let mut planted_sim = Sim::new(7, Faults::default());
        planted_sim.fill(dir_name, 0);
        with_brand(|brand| {
            let mut handler = SimHandler::new(&mut planted_sim, TaskId(11), brand, alloc);
            let root = handler.root();
            let record_buf = DmaBuf::from_slice_in(&payload, alloc);
            let machine = CasRecordPublishMachine::with_plant(
                CasPublishOpts {
                    dir: root,
                    key,
                    tmp: tmp.clone(),
                    final_name: final_name.clone(),
                    record_buf,
                },
                CasPublishPlant::SkipCloseOnError,
            );
            let res = drive(&mut handler, &mut (), machine, DriveOpts { max_steps: 16 });
            assert!(
                matches!(res, Err(DriveError::Machine(CasPublishError::Fs(FsError::NoSpace { .. })))),
                "expected NoSpace fault on WriteFrom, got {res:?}"
            );
            let finish = handler.finish_task(ExitKind::Normal);
            assert!(
                matches!(
                    finish,
                    Err(DriverHandlerError::LeakedHandles(leaked))
                        if leaked.task == TaskId(11) && leaked.count == 1
                ),
                "expected DriverHandlerError::LeakedHandles for SkipCloseOnError, got {finish:?}"
            );
        });
    }
}
