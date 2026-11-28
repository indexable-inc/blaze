//! Tests and deterministic simulation proofs for `blaze-machine`:
//! 1. Real explicit `HttpExchange` transport machine (partial sends, partial reads, `Race<Recv, Timer>`
//!    timeout, mandatory linear `Close` on BOTH `Ok` and `Err` paths) driven by `Script` + `blaze-sim`,
//!    with planted-bug controls (short-send bug and skipped-close-on-error bug).
//! 2. `TaskFrame` zero-`Pin` cross-thread move with `FrameVec` + DMA-drain cancellation DST with
//!    planted-bug control (`ResetWhileDmaInFlight`).
//! 3. `Batch<R, 4>` fan-out request and `Call` / `run_sub` sub-machine composition.

use core::num::NonZeroU32;
use std::thread;

use blaze_own::testing::heap;
use blaze_own::{Alloc, BrandedBump, FixedString, FixedVec, FrameVec, Inline, Vec, with_brand};
use blaze_sim::{Broke, Config};

use crate::{
    Batch, BatchReady, BufferRing, CancelAction, CancelReason, Chain, ChainOutcome, DriveError,
    DriveOpts, EdgeKind, GraceBudget, GraphEdge, GraphError, GraphPlant, HandleOp, HandleSlot,
    Handler, Join, LeaseError, LeasePlant, LeasedBuf, Machine, MachineGraph, Perform, Race,
    RaceOutcome, RecordHandler, Request, ReturnBuf, RingBufId, RingGroupId, ScopeBudget, Script,
    ScriptError, StateId, StateNode, Stopped, TaskFrame, TaskFramePlant, TranscriptEntry, Turn,
    drive, emit_lean, run_sub, verify_graph, verify_graph_with,
};

// ---------------------------------------------------------------------------
// 1. Real explicit transport machine (`HttpExchange`) with planted-bug controls
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ConnToken {
    slot: u32,
    generation: NonZeroU32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TransportReq {
    Send { conn: ConnToken, offset: usize, len: usize },
    RecvWithTimeout { conn: ConnToken, timeout_ms: u32 },
    Close { conn: ConnToken },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NetErr {
    Reset,
    TimedOut,
    CloseFailed,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum TransportAns {
    Sent(Result<usize, NetErr>),
    RecvOrTimeout(RaceOutcome<Result<usize, NetErr>, ()>),
    Closed(Result<(), NetErr>),
}

impl Request for TransportReq {
    type Answer<'s> = TransportAns;
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum ExchangePlant {
    #[default]
    None,
    /// Planted bug: assume full send on partial write (`sent = total_len` instead of `sent + n`).
    IgnoreShortSend,
    /// Planted bug: return `Fail(err)` directly on send/recv error without closing `conn`.
    SkipCloseOnError,
}

enum HttpExchange {
    Init { conn: ConnToken, req_len: usize, resp_target: usize, plant: ExchangePlant },
    Sending { conn: ConnToken, sent: usize, req_len: usize, resp_target: usize, plant: ExchangePlant },
    Receiving { conn: ConnToken, got: usize, resp_target: usize, plant: ExchangePlant },
    Closing { outcome: Result<usize, NetErr> },
}

impl HttpExchange {
    fn new(conn: ConnToken, req_len: usize, resp_target: usize, plant: ExchangePlant) -> Self {
        Self::Init { conn, req_len, resp_target, plant }
    }
}

impl Machine for HttpExchange {
    type Req = TransportReq;
    type Out = usize;
    type Err = NetErr;

    fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match self {
            HttpExchange::Init { conn, req_len, resp_target, plant } => Turn::Ask {
                request: TransportReq::Send { conn, offset: 0, len: req_len },
                next: HttpExchange::Sending { conn, sent: 0, req_len, resp_target, plant },
            },
            _ => unreachable!("start called on non-Init state"),
        }
    }

    fn resume(self, answer: TransportAns, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match (self, answer) {
            (
                HttpExchange::Sending { conn, sent, req_len, resp_target, plant },
                TransportAns::Sent(Ok(n)),
            ) => {
                let next_sent = if plant == ExchangePlant::IgnoreShortSend {
                    req_len
                } else {
                    sent.saturating_add(n)
                };
                if next_sent < req_len {
                    Turn::Ask {
                        request: TransportReq::Send { conn, offset: next_sent, len: req_len - next_sent },
                        next: HttpExchange::Sending { conn, sent: next_sent, req_len, resp_target, plant },
                    }
                } else {
                    Turn::Ask {
                        request: TransportReq::RecvWithTimeout { conn, timeout_ms: 500 },
                        next: HttpExchange::Receiving { conn, got: 0, resp_target, plant },
                    }
                }
            }
            (
                HttpExchange::Sending { conn, plant, .. },
                TransportAns::Sent(Err(e)),
            ) => {
                if plant == ExchangePlant::SkipCloseOnError {
                    Turn::Fail(e)
                } else {
                    Turn::Ask {
                        request: TransportReq::Close { conn },
                        next: HttpExchange::Closing { outcome: Err(e) },
                    }
                }
            }
            (
                HttpExchange::Receiving { conn, got, resp_target, plant },
                TransportAns::RecvOrTimeout(RaceOutcome::First(Ok(n))),
            ) => {
                let next_got = got.saturating_add(n);
                if next_got < resp_target && n > 0 {
                    Turn::Ask {
                        request: TransportReq::RecvWithTimeout { conn, timeout_ms: 500 },
                        next: HttpExchange::Receiving { conn, got: next_got, resp_target, plant },
                    }
                } else {
                    Turn::Ask {
                        request: TransportReq::Close { conn },
                        next: HttpExchange::Closing { outcome: Ok(next_got) },
                    }
                }
            }
            (
                HttpExchange::Receiving { conn, plant, .. },
                TransportAns::RecvOrTimeout(RaceOutcome::First(Err(e))),
            ) => {
                if plant == ExchangePlant::SkipCloseOnError {
                    Turn::Fail(e)
                } else {
                    Turn::Ask {
                        request: TransportReq::Close { conn },
                        next: HttpExchange::Closing { outcome: Err(e) },
                    }
                }
            }
            (
                HttpExchange::Receiving { conn, plant, .. },
                TransportAns::RecvOrTimeout(RaceOutcome::Second(())),
            ) => {
                if plant == ExchangePlant::SkipCloseOnError {
                    Turn::Fail(NetErr::TimedOut)
                } else {
                    Turn::Ask {
                        request: TransportReq::Close { conn },
                        next: HttpExchange::Closing { outcome: Err(NetErr::TimedOut) },
                    }
                }
            }
            (HttpExchange::Closing { outcome }, TransportAns::Closed(close_res)) => {
                match (outcome, close_res) {
                    (Ok(n), Ok(())) => Turn::Done(n),
                    (Ok(_), Err(close_err)) => Turn::Fail(close_err),
                    (Err(io_err), _) => Turn::Fail(io_err),
                }
            }
            _ => unreachable!("answer always matches active waiting state"),
        }
    }

    fn on_cancel(
        self,
        _stopped: Stopped,
        _cx: &mut (),
    ) -> CancelAction<Turn<Self, Self::Req, Self::Out, Self::Err>> {
        match self {
            HttpExchange::Sending { conn, .. } | HttpExchange::Receiving { conn, .. } => {
                CancelAction::Cleanup(Turn::Ask {
                    request: TransportReq::Close { conn },
                    next: HttpExchange::Closing { outcome: Err(NetErr::Reset) },
                })
            }
            _ => CancelAction::Abandon,
        }
    }
}

#[test]
fn http_exchange_handles_partial_writes_reads_timeout_and_closes_handle() {
    let h = heap();
    let conn = ConnToken { slot: 0, generation: NonZeroU32::new(1).unwrap() };

    // Partial send (4 + 6 = 10), partial recv (3 + 5 = 8), then linear Close -> Done(8).
    let mut script = Script::new_in(h.alloc());
    script.expect_answer(TransportReq::Send { conn, offset: 0, len: 10 }, TransportAns::Sent(Ok(4)));
    script.expect_answer(TransportReq::Send { conn, offset: 4, len: 6 }, TransportAns::Sent(Ok(6)));
    script.expect_answer(
        TransportReq::RecvWithTimeout { conn, timeout_ms: 500 },
        TransportAns::RecvOrTimeout(RaceOutcome::First(Ok(3))),
    );
    script.expect_answer(
        TransportReq::RecvWithTimeout { conn, timeout_ms: 500 },
        TransportAns::RecvOrTimeout(RaceOutcome::First(Ok(5))),
    );
    script.expect_answer(TransportReq::Close { conn }, TransportAns::Closed(Ok(())));

    let res = drive(
        &mut script,
        &mut (),
        HttpExchange::new(conn, 10, 8, ExchangePlant::None),
        DriveOpts { max_steps: 16 },
    );
    assert_eq!(res, Ok(8));
    assert_eq!(script.finish(), Ok(()));

    // Timeout on recv must still close the linear handle before failing with NetErr::TimedOut.
    let mut timeout_script = Script::new_in(h.alloc());
    timeout_script.expect_answer(TransportReq::Send { conn, offset: 0, len: 5 }, TransportAns::Sent(Ok(5)));
    timeout_script.expect_answer(
        TransportReq::RecvWithTimeout { conn, timeout_ms: 500 },
        TransportAns::RecvOrTimeout(RaceOutcome::Second(())),
    );
    timeout_script.expect_answer(TransportReq::Close { conn }, TransportAns::Closed(Ok(())));

    let res = drive(
        &mut timeout_script,
        &mut (),
        HttpExchange::new(conn, 5, 16, ExchangePlant::None),
        DriveOpts { max_steps: 16 },
    );
    assert_eq!(res, Err(DriveError::Machine(NetErr::TimedOut)));
    assert_eq!(timeout_script.finish(), Ok(()));

    // External cancel runs bounded graceful cleanup (`Close`).
    let mut cancel_script = Script::new_in(h.alloc());
    cancel_script.expect_stop(
        TransportReq::Send { conn, offset: 0, len: 5 },
        Stopped::cancelled(GraceBudget::DEFAULT),
    );
    cancel_script.expect_answer(TransportReq::Close { conn }, TransportAns::Closed(Ok(())));
    let res = drive(
        &mut cancel_script,
        &mut (),
        HttpExchange::new(conn, 5, 16, ExchangePlant::None),
        DriveOpts { max_steps: 16 },
    );
    assert_eq!(res, Err(DriveError::Machine(NetErr::Reset)));
    assert_eq!(cancel_script.finish(), Ok(()));
}

#[test]
fn the_oracle_catches_short_send_mishandling() {
    let h = heap();
    let conn = ConnToken { slot: 0, generation: NonZeroU32::new(1).unwrap() };
    let mut script = Script::new_in(h.alloc());
    script.expect_answer(TransportReq::Send { conn, offset: 0, len: 10 }, TransportAns::Sent(Ok(4)));
    script.expect_answer(TransportReq::Send { conn, offset: 4, len: 6 }, TransportAns::Sent(Ok(6)));
    let res = drive(
        &mut script,
        &mut (),
        HttpExchange::new(conn, 10, 8, ExchangePlant::IgnoreShortSend),
        DriveOpts { max_steps: 16 },
    );
    assert!(matches!(
        res,
        Err(DriveError::Handler(ScriptError::Mismatch { step: 1, .. }))
    ));
}

#[test]
fn the_oracle_catches_skipped_close_on_error() {
    let h = heap();
    let conn = ConnToken { slot: 0, generation: NonZeroU32::new(1).unwrap() };
    let mut script = Script::new_in(h.alloc());
    script.expect_answer(TransportReq::Send { conn, offset: 0, len: 10 }, TransportAns::Sent(Err(NetErr::Reset)));
    script.expect_answer(TransportReq::Close { conn }, TransportAns::Closed(Ok(())));

    let res = drive(
        &mut script,
        &mut (),
        HttpExchange::new(conn, 10, 8, ExchangePlant::SkipCloseOnError),
        DriveOpts { max_steps: 16 },
    );
    assert_eq!(res, Err(DriveError::Machine(NetErr::Reset)));
    assert_eq!(script.finish(), Err(ScriptError::UnconsumedSteps { step: 1, remaining: 1 }));
}

// ---------------------------------------------------------------------------
// 2. TaskFrame cross-thread move (zero Pin) + DMA cancellation drain DST
// ---------------------------------------------------------------------------

#[test]
fn task_frame_moves_across_threads_without_pin() {
    let h = heap();
    let frame = TaskFrame::new(Inline::<256>::new(), 41u64);

    // Move `TaskFrame` across an OS thread boundary: proves `Send` with zero `Pin`.
    let handle = thread::spawn(move || {
        let (arena, state) = frame.try_into_parts().expect("no DMA in flight");
        with_brand(|id| {
            let bump = BrandedBump::new(&arena, id);
            let mut buf: FrameVec<'_, u8> = FrameVec::new();
            buf.push(1, &bump);
            buf.push(2, &bump);
            assert_eq!(buf.as_slice(), &[1, 2]);
        });
        state + 1
    });
    assert_eq!(handle.join().expect("thread finished"), 42);
    let _ = h;
}

/// The invariant: a `TaskFrame` arena is NEVER reset while any submitted or cancelled DMA buffer
/// remains in flight; once every `DmaTicket` drains, reset succeeds cleanly.
fn run_task_frame_dma_dst(plant: TaskFramePlant) {
    let h = heap();
    blaze_sim::check(Config::cases(200), |tape| {
        let mut frame = TaskFrame::with_plant(0u32, (), plant);
        let mut live: Vec<crate::DmaTicket, Alloc<'_>> = Vec::new_in(h.alloc());
        let ops = tape.below(32, "ops");
        for _ in 0..ops {
            let action = tape.below(4, "action");
            match action {
                0 => {
                    // Submit a new DMA read/write buffer from this frame.
                    if let Ok(ticket) = frame.start_dma() {
                        live.push(ticket);
                    }
                }
                1 => {
                    // Kernel CQE arrives for one in-flight (or cancelled) DMA op in any order.
                    if !live.is_empty() {
                        let idx = (tape.below(live.len() as u64, "complete_idx") as usize)
                            .min(live.len() - 1);
                        let ticket = live.swap_remove(idx);
                        frame.complete_dma(ticket);
                    }
                }
                2 => {
                    // Attempt to reset the arena (e.g. immediately after cancellation).
                    let was_busy = !live.is_empty();
                    let res = frame.try_reset(|epoch| *epoch += 1);
                    if was_busy && res.is_ok() {
                        return Err(Broke::Oracle(
                            "TaskFrame reset succeeded while DMA buffer was still in flight (DMA UAF)".into(),
                        ));
                    }
                    if !was_busy && res.is_err() {
                        return Err(Broke::Oracle(
                            "TaskFrame refused reset even though all DMA buffers had drained".into(),
                        ));
                    }
                }
                _ => {
                    // Drain all remaining tickets then verify reset succeeds.
                    while let Some(ticket) = live.pop() {
                        frame.complete_dma(ticket);
                    }
                    if frame.try_reset(|epoch| *epoch += 1).is_err() {
                        return Err(Broke::Oracle("drained TaskFrame refused reset".into()));
                    }
                }
            }
        }
        // Final drain.
        while let Some(ticket) = live.pop() {
            frame.complete_dma(ticket);
        }
        if frame.in_flight_dma() != 0 {
            return Err(Broke::Oracle("in_flight_dma counter leaked after full drain".into()));
        }
        Ok(())
    });
}

#[test]
fn task_frame_dma_cancellation_drain_invariant() {
    run_task_frame_dma_dst(TaskFramePlant::None);
}

#[test]
#[should_panic(expected = "DMA UAF")]
fn the_oracle_catches_frame_reset_with_in_flight_dma() {
    run_task_frame_dma_dst(TaskFramePlant::ResetWhileDmaInFlight);
}

// ---------------------------------------------------------------------------
// 3. Batch<R, 4> fan-out request and sub-machine composition (`run_sub`)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct StatReq(u32);

impl Request for StatReq {
    type Answer<'s> = u64;
}

struct FanOutHandler;
impl Handler<Batch<StatReq, 4>> for FanOutHandler {
    type Error = ();

    fn perform<Out>(
        &mut self,
        request: Batch<StatReq, 4>,
        deliver: impl FnOnce(Perform<FixedVec<BatchReady<u64>, 4>>) -> Out,
    ) -> Result<Out, Self::Error> {
        let mut out: FixedVec<BatchReady<u64>, 4> = FixedVec::new();
        for (i, req) in request.into_items().as_slice().iter().enumerate() {
            let _ = out.push(BatchReady { index: i as u16, answer: u64::from(req.0) * 10 });
        }
        Ok(deliver(Perform::Answered(out)))
    }
}

enum StatBatchMachine {
    Start,
    Waiting,
}

impl Machine for StatBatchMachine {
    type Req = Batch<StatReq, 4>;
    type Out = u64;
    type Err = ();

    fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        let mut batch: Batch<StatReq, 4> = Batch::new();
        for id in [1u32, 2, 3, 4] {
            assert!(batch.try_push(StatReq(id)).is_ok());
        }
        assert!(batch.try_push(StatReq(5)).is_err(), "bounded capacity 4 refuses 5th item");
        Turn::Ask { request: batch, next: StatBatchMachine::Waiting }
    }

    fn resume(
        self,
        answer: FixedVec<BatchReady<u64>, 4>,
        _cx: &mut (),
    ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        let sum: u64 = answer.as_slice().iter().map(|r| r.answer).sum();
        Turn::Done(sum)
    }
}

#[test]
fn batch_fan_out_and_sub_machine_composition_work() {
    let mut handler = FanOutHandler;
    let total = run_sub(&mut handler, &mut (), StatBatchMachine::Start, DriveOpts { max_steps: 4 });
    assert_eq!(total, Ok(100));

    // Also verify `Race` request type plumbing cleanly compiles and matches both arms.
    let race = Race::new(StatReq(1), StatReq(2));
    let (a, b) = race.split();
    assert_eq!((a.0, b.0), (1, 2));
}

// ---------------------------------------------------------------------------
// 4. Linked `Chain<A, B>` (`IOSQE_IO_LINK`) request over `Script`
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct WriteReq {
    fd: u32,
    bytes: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FlushReq {
    fd: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DiskErr {
    NoSpace,
    FlushFailed,
    ShortCircuited,
}

impl Request for WriteReq {
    type Answer<'s> = Result<usize, DiskErr>;
}

impl Request for FlushReq {
    type Answer<'s> = Result<(), DiskErr>;
}

enum DurableWriteMachine {
    Start { fd: u32, bytes: usize },
    Waiting,
}

impl Machine for DurableWriteMachine {
    type Req = Chain<WriteReq, FlushReq>;
    type Out = usize;
    type Err = DiskErr;

    fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match self {
            DurableWriteMachine::Start { fd, bytes } => Turn::Ask {
                request: Chain::new(WriteReq { fd, bytes }, FlushReq { fd }),
                next: DurableWriteMachine::Waiting,
            },
            DurableWriteMachine::Waiting => unreachable!("start called on Waiting state"),
        }
    }

    fn resume(
        self,
        answer: ChainOutcome<Result<usize, DiskErr>, Result<(), DiskErr>>,
        _cx: &mut (),
    ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match answer {
            ChainOutcome::Both(Ok(n), Ok(())) => Turn::Done(n),
            ChainOutcome::Both(Ok(_), Err(e))
            | ChainOutcome::Both(Err(e), _)
            | ChainOutcome::FirstOnly(Err(e)) => Turn::Fail(e),
            ChainOutcome::FirstOnly(Ok(_)) => Turn::Fail(DiskErr::ShortCircuited),
        }
    }
}

#[test]
fn chain_request_handles_both_and_first_only_short_circuit_over_script() {
    let h = heap();

    // Verify `Chain::new` and `Chain::split`.
    let chain = Chain::new(WriteReq { fd: 7, bytes: 64 }, FlushReq { fd: 7 });
    let (w, fl) = chain.split();
    assert_eq!(w, WriteReq { fd: 7, bytes: 64 });
    assert_eq!(fl, FlushReq { fd: 7 });

    // Happy path: both linked SQEs succeed (`ChainOutcome::Both`).
    let mut ok_script = Script::new_in(h.alloc());
    ok_script.expect_answer(
        Chain::new(WriteReq { fd: 3, bytes: 128 }, FlushReq { fd: 3 }),
        ChainOutcome::Both(Ok(128), Ok(())),
    );
    let res = drive(
        &mut ok_script,
        &mut (),
        DurableWriteMachine::Start { fd: 3, bytes: 128 },
        DriveOpts { max_steps: 4 },
    );
    assert_eq!(res, Ok(128));
    assert_eq!(ok_script.finish(), Ok(()));

    // Short-circuit path: `WriteReq` fails with `NoSpace`, so `FlushReq` is skipped/ECANCELED
    // and the driver delivers `ChainOutcome::FirstOnly`.
    let mut fail_script = Script::new_in(h.alloc());
    fail_script.expect_answer(
        Chain::new(WriteReq { fd: 3, bytes: 128 }, FlushReq { fd: 3 }),
        ChainOutcome::FirstOnly(Err(DiskErr::NoSpace)),
    );
    let res = drive(
        &mut fail_script,
        &mut (),
        DurableWriteMachine::Start { fd: 3, bytes: 128 },
        DriveOpts { max_steps: 4 },
    );
    assert_eq!(res, Err(DriveError::Machine(DiskErr::NoSpace)));
    assert_eq!(fail_script.finish(), Ok(()));

    // Second linked op fails after first succeeds.
    let mut flush_err_script = Script::new_in(h.alloc());
    flush_err_script.expect_answer(
        Chain::new(WriteReq { fd: 3, bytes: 128 }, FlushReq { fd: 3 }),
        ChainOutcome::Both(Ok(128), Err(DiskErr::FlushFailed)),
    );
    let res = drive(
        &mut flush_err_script,
        &mut (),
        DurableWriteMachine::Start { fd: 3, bytes: 128 },
        DriveOpts { max_steps: 4 },
    );
    assert_eq!(res, Err(DriveError::Machine(DiskErr::FlushFailed)));
    assert_eq!(flush_err_script.finish(), Ok(()));
}

// ---------------------------------------------------------------------------
// 5. Machine state-graph verifier + Lean 4 emitter + planted-bug controls
// ---------------------------------------------------------------------------

#[test]
fn verify_graph_valid_open_write_flush_close_and_emit_lean() {
    const INIT: StateId = StateId(0);
    const WRITING: StateId = StateId(1);
    const FLUSHING: StateId = StateId(2);
    const CLOSING: StateId = StateId(3);
    const FILE: HandleSlot = HandleSlot(0);

    const STATES: [StateNode; 4] = [
        StateNode { id: INIT, name: "Init", loop_state: false, initial_handles: 0 },
        StateNode { id: WRITING, name: "Writing", loop_state: false, initial_handles: 0 },
        StateNode { id: FLUSHING, name: "Flushing", loop_state: false, initial_handles: 0 },
        StateNode { id: CLOSING, name: "Closing", loop_state: false, initial_handles: 0 },
    ];
    const EDGES: [GraphEdge; 10] = [
        GraphEdge {
            from: INIT,
            kind: EdgeKind::Ask { to: WRITING },
            handle_op: HandleOp::Open(FILE),
            label: "open_ok",
        },
        GraphEdge {
            from: INIT,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "open_err",
        },
        GraphEdge {
            from: WRITING,
            kind: EdgeKind::Ask { to: WRITING },
            handle_op: HandleOp::None,
            label: "write_partial",
        },
        GraphEdge {
            from: WRITING,
            kind: EdgeKind::Ask { to: FLUSHING },
            handle_op: HandleOp::None,
            label: "write_done",
        },
        GraphEdge {
            from: WRITING,
            kind: EdgeKind::CancelCleanup { to: CLOSING },
            handle_op: HandleOp::Close(FILE),
            label: "write_cancel_close",
        },
        GraphEdge {
            from: FLUSHING,
            kind: EdgeKind::Ask { to: CLOSING },
            handle_op: HandleOp::Close(FILE),
            label: "flush_then_close",
        },
        GraphEdge {
            from: FLUSHING,
            kind: EdgeKind::CancelCleanup { to: CLOSING },
            handle_op: HandleOp::Close(FILE),
            label: "flush_cancel_close",
        },
        GraphEdge {
            from: CLOSING,
            kind: EdgeKind::Done,
            handle_op: HandleOp::None,
            label: "close_ok",
        },
        GraphEdge {
            from: CLOSING,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "close_err",
        },
        GraphEdge {
            from: CLOSING,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "close_abandon",
        },
    ];

    let graph = MachineGraph {
        name: "DurableWrite",
        entry: INIT,
        grace_max_steps: 2,
        states: &STATES,
        edges: &EDGES,
    };
    assert_eq!(verify_graph(&graph), Ok(()));

    let mut lean = FixedString::<4096>::new();
    assert_eq!(emit_lean(&graph, &mut lean), Ok(()));
    let text = lean.text().expect("Lean output fits in 4096 bytes");
    assert!(text.contains("namespace DurableWrite"), "missing namespace:\n{text}");
    assert!(text.contains("inductive State where"), "missing inductive State:\n{text}");
    assert!(
        text.contains("inductive Step : State -> Nat -> State -> Nat -> Prop where"),
        "missing inductive Step:\n{text}"
    );
    assert!(text.contains("def legalTerminal"), "missing legalTerminal:\n{text}");
    assert!(
        text.contains("theorem verified_machine_no_leak"),
        "missing verified_machine_no_leak:\n{text}"
    );
    assert!(text.contains("| Writing"), "missing Writing constructor:\n{text}");
    assert!(text.contains("end DurableWrite"), "missing end namespace:\n{text}");
}

#[test]
fn verify_graph_accepts_abandon_with_live_handle_and_transfer_out() {
    const INIT: StateId = StateId(0);
    const OPENED: StateId = StateId(1);
    const FILE: HandleSlot = HandleSlot(0);

    const STATES: [StateNode; 2] = [
        StateNode { id: INIT, name: "Init", loop_state: false, initial_handles: 0 },
        StateNode { id: OPENED, name: "Opened", loop_state: false, initial_handles: 0 },
    ];
    const EDGES: [GraphEdge; 3] = [
        GraphEdge {
            from: INIT,
            kind: EdgeKind::Ask { to: OPENED },
            handle_op: HandleOp::Open(FILE),
            label: "open",
        },
        // External `Stopped` abandons the live handle to the driver's HandleTable.
        GraphEdge {
            from: OPENED,
            kind: EdgeKind::Abandon,
            handle_op: HandleOp::None,
            label: "abandon_live",
        },
        // Normal `Done` transfers ownership of the handle out to the caller.
        GraphEdge {
            from: OPENED,
            kind: EdgeKind::Done,
            handle_op: HandleOp::TransferOut(FILE),
            label: "return_file",
        },
    ];

    let graph = MachineGraph {
        name: "OpenAndReturn",
        entry: INIT,
        grace_max_steps: 1,
        states: &STATES,
        edges: &EDGES,
    };
    assert_eq!(verify_graph(&graph), Ok(()));
}

#[test]
fn verify_graph_catches_unmarked_cycle_and_plant_control_misses_it() {
    const INIT: StateId = StateId(0);
    const PING: StateId = StateId(1);
    const PONG: StateId = StateId(2);

    const CYCLIC_STATES: [StateNode; 3] = [
        StateNode { id: INIT, name: "Init", loop_state: false, initial_handles: 0 },
        StateNode { id: PING, name: "Ping", loop_state: false, initial_handles: 0 },
        StateNode { id: PONG, name: "Pong", loop_state: false, initial_handles: 0 },
    ];
    const EDGES: [GraphEdge; 3] = [
        GraphEdge {
            from: INIT,
            kind: EdgeKind::Ask { to: PING },
            handle_op: HandleOp::None,
            label: "start",
        },
        GraphEdge {
            from: PING,
            kind: EdgeKind::Ask { to: PONG },
            handle_op: HandleOp::None,
            label: "ping",
        },
        GraphEdge {
            from: PONG,
            kind: EdgeKind::Ask { to: PING },
            handle_op: HandleOp::None,
            label: "pong",
        },
    ];

    let cyclic = MachineGraph {
        name: "UnmarkedSpin",
        entry: INIT,
        grace_max_steps: 4,
        states: &CYCLIC_STATES,
        edges: &EDGES,
    };
    assert_eq!(verify_graph(&cyclic), Err(GraphError::UnmarkedCycle(PING)));
    // Planted-bug control: SkipCycleCheck misses the unmarked cycle.
    assert_eq!(verify_graph_with(&cyclic, GraphPlant::SkipCycleCheck), Ok(()));

    // Marking one state in the cycle `loop_state: true` explicitly accepts an intentional loop.
    const LOOP_STATES: [StateNode; 3] = [
        StateNode { id: INIT, name: "Init", loop_state: false, initial_handles: 0 },
        StateNode { id: PING, name: "Ping", loop_state: true, initial_handles: 0 },
        StateNode { id: PONG, name: "Pong", loop_state: false, initial_handles: 0 },
    ];
    let intentional_loop = MachineGraph {
        name: "ServerLoop",
        entry: INIT,
        grace_max_steps: 4,
        states: &LOOP_STATES,
        edges: &EDGES,
    };
    assert_eq!(verify_graph(&intentional_loop), Ok(()));
}

#[test]
fn verify_graph_catches_handle_leak_on_fail_and_done_with_plant_control() {
    const INIT: StateId = StateId(0);
    const WORKING: StateId = StateId(1);
    const FILE: HandleSlot = HandleSlot(2);

    const STATES: [StateNode; 2] = [
        StateNode { id: INIT, name: "Init", loop_state: false, initial_handles: 0 },
        StateNode { id: WORKING, name: "Working", loop_state: false, initial_handles: 0 },
    ];

    // Leak on Fail.
    const LEAK_ON_FAIL_EDGES: [GraphEdge; 3] = [
        GraphEdge {
            from: INIT,
            kind: EdgeKind::Ask { to: WORKING },
            handle_op: HandleOp::Open(FILE),
            label: "open",
        },
        GraphEdge {
            from: WORKING,
            kind: EdgeKind::Done,
            handle_op: HandleOp::Close(FILE),
            label: "ok_close",
        },
        GraphEdge {
            from: WORKING,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "fail_leak",
        },
    ];
    let leak_fail = MachineGraph {
        name: "LeakOnFail",
        entry: INIT,
        grace_max_steps: 2,
        states: &STATES,
        edges: &LEAK_ON_FAIL_EDGES,
    };
    assert_eq!(
        verify_graph(&leak_fail),
        Err(GraphError::HandleLeakedOnFail { from: WORKING, live_mask: 1 << 2 })
    );
    // Planted-bug control: SkipLeakCheck misses the leaked handle on Fail.
    assert_eq!(verify_graph_with(&leak_fail, GraphPlant::SkipLeakCheck), Ok(()));

    // Leak on Done.
    const LEAK_ON_DONE_EDGES: [GraphEdge; 2] = [
        GraphEdge {
            from: INIT,
            kind: EdgeKind::Ask { to: WORKING },
            handle_op: HandleOp::Open(FILE),
            label: "open",
        },
        GraphEdge {
            from: WORKING,
            kind: EdgeKind::Done,
            handle_op: HandleOp::None,
            label: "done_leak",
        },
    ];
    let leak_done = MachineGraph {
        name: "LeakOnDone",
        entry: INIT,
        grace_max_steps: 2,
        states: &STATES,
        edges: &LEAK_ON_DONE_EDGES,
    };
    assert_eq!(
        verify_graph(&leak_done),
        Err(GraphError::HandleLeakedOnDone { from: WORKING, live_mask: 1 << 2 })
    );
    assert_eq!(verify_graph_with(&leak_done, GraphPlant::SkipLeakCheck), Ok(()));
}

#[test]
fn verify_graph_catches_unbounded_cancel_cleanup_with_plant_control() {
    const INIT: StateId = StateId(0);
    const C1: StateId = StateId(1);
    const C2: StateId = StateId(2);
    const C3: StateId = StateId(3);

    const STATES: [StateNode; 4] = [
        StateNode { id: INIT, name: "Init", loop_state: false, initial_handles: 0 },
        StateNode { id: C1, name: "Cleanup1", loop_state: false, initial_handles: 0 },
        StateNode { id: C2, name: "Cleanup2", loop_state: false, initial_handles: 0 },
        StateNode { id: C3, name: "Cleanup3", loop_state: false, initial_handles: 0 },
    ];

    // 3-step cleanup chain exceeds `grace_max_steps: 2`.
    const LONG_CLEANUP: [GraphEdge; 5] = [
        GraphEdge {
            from: INIT,
            kind: EdgeKind::Done,
            handle_op: HandleOp::None,
            label: "done",
        },
        GraphEdge {
            from: INIT,
            kind: EdgeKind::CancelCleanup { to: C1 },
            handle_op: HandleOp::None,
            label: "c1",
        },
        GraphEdge {
            from: C1,
            kind: EdgeKind::CancelCleanup { to: C2 },
            handle_op: HandleOp::None,
            label: "c2",
        },
        GraphEdge {
            from: C2,
            kind: EdgeKind::CancelCleanup { to: C3 },
            handle_op: HandleOp::None,
            label: "c3",
        },
        GraphEdge {
            from: C3,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "c_end",
        },
    ];
    let long_graph = MachineGraph {
        name: "TooLongCleanup",
        entry: INIT,
        grace_max_steps: 2,
        states: &STATES,
        edges: &LONG_CLEANUP,
    };
    assert_eq!(
        verify_graph(&long_graph),
        Err(GraphError::UnboundedCancelCleanup { start: INIT, steps: 3, max_steps: 2 })
    );
    // Planted-bug control: SkipCancelBoundCheck misses the unbounded cleanup chain.
    assert_eq!(verify_graph_with(&long_graph, GraphPlant::SkipCancelBoundCheck), Ok(()));

    // Cyclic cleanup loop caught even with a generous `grace_max_steps`.
    const CYCLIC_CLEANUP: [GraphEdge; 6] = [
        GraphEdge {
            from: INIT,
            kind: EdgeKind::Done,
            handle_op: HandleOp::None,
            label: "done",
        },
        GraphEdge {
            from: INIT,
            kind: EdgeKind::CancelCleanup { to: C1 },
            handle_op: HandleOp::None,
            label: "c1",
        },
        GraphEdge {
            from: C1,
            kind: EdgeKind::CancelCleanup { to: C2 },
            handle_op: HandleOp::None,
            label: "c2",
        },
        GraphEdge {
            from: C2,
            kind: EdgeKind::CancelCleanup { to: C1 },
            handle_op: HandleOp::None,
            label: "c_loop",
        },
        GraphEdge {
            from: C2,
            kind: EdgeKind::CancelCleanup { to: C3 },
            handle_op: HandleOp::None,
            label: "c3",
        },
        GraphEdge {
            from: C3,
            kind: EdgeKind::Fail,
            handle_op: HandleOp::None,
            label: "c_end",
        },
    ];
    let cyclic_cleanup = MachineGraph {
        name: "CyclicCleanup",
        entry: INIT,
        grace_max_steps: 16,
        states: &STATES,
        edges: &CYCLIC_CLEANUP,
    };
    assert_eq!(
        verify_graph(&cyclic_cleanup),
        Err(GraphError::UnboundedCancelCleanup { start: INIT, steps: 17, max_steps: 16 })
    );
    assert_eq!(verify_graph_with(&cyclic_cleanup, GraphPlant::SkipCancelBoundCheck), Ok(()));
}

#[test]
fn verify_graph_catches_structural_and_handle_errors() {
    const S0: StateId = StateId(0);
    const S1: StateId = StateId(1);
    const S2: StateId = StateId(2);
    const MISSING: StateId = StateId(99);
    const H0: HandleSlot = HandleSlot(0);

    const TWO_STATES: [StateNode; 2] = [
        StateNode { id: S0, name: "S0", loop_state: false, initial_handles: 0 },
        StateNode { id: S1, name: "S1", loop_state: false, initial_handles: 0 },
    ];

    // 1. InvalidEntry.
    let bad_entry = MachineGraph {
        name: "BadEntry",
        entry: MISSING,
        grace_max_steps: 1,
        states: &TWO_STATES,
        edges: &[],
    };
    assert_eq!(verify_graph(&bad_entry), Err(GraphError::InvalidEntry(MISSING)));

    // 2. InvalidEdgeTarget.
    const BAD_TARGET_EDGES: [GraphEdge; 1] = [GraphEdge {
        from: S0,
        kind: EdgeKind::Ask { to: MISSING },
        handle_op: HandleOp::None,
        label: "dangling",
    }];
    let bad_target = MachineGraph {
        name: "BadTarget",
        entry: S0,
        grace_max_steps: 1,
        states: &TWO_STATES,
        edges: &BAD_TARGET_EDGES,
    };
    assert_eq!(
        verify_graph(&bad_target),
        Err(GraphError::InvalidEdgeTarget { from: S0, to: MISSING })
    );

    // 3. UnreachableState.
    const UNREACHABLE_EDGES: [GraphEdge; 1] = [GraphEdge {
        from: S0,
        kind: EdgeKind::Done,
        handle_op: HandleOp::None,
        label: "done",
    }];
    let unreachable_graph = MachineGraph {
        name: "Unreachable",
        entry: S0,
        grace_max_steps: 1,
        states: &TWO_STATES,
        edges: &UNREACHABLE_EDGES,
    };
    assert_eq!(verify_graph(&unreachable_graph), Err(GraphError::UnreachableState(S1)));

    // 4. DeadEnd.
    const DEAD_END_EDGES: [GraphEdge; 1] = [GraphEdge {
        from: S0,
        kind: EdgeKind::Ask { to: S1 },
        handle_op: HandleOp::None,
        label: "go",
    }];
    let dead_end = MachineGraph {
        name: "DeadEnd",
        entry: S0,
        grace_max_steps: 1,
        states: &TWO_STATES,
        edges: &DEAD_END_EDGES,
    };
    assert_eq!(verify_graph(&dead_end), Err(GraphError::DeadEnd(S1)));

    // 5. DoubleOpen.
    const DOUBLE_OPEN_EDGES: [GraphEdge; 2] = [
        GraphEdge {
            from: S0,
            kind: EdgeKind::Ask { to: S1 },
            handle_op: HandleOp::Open(H0),
            label: "open1",
        },
        GraphEdge {
            from: S1,
            kind: EdgeKind::Done,
            handle_op: HandleOp::Open(H0),
            label: "open2",
        },
    ];
    let double_open = MachineGraph {
        name: "DoubleOpen",
        entry: S0,
        grace_max_steps: 1,
        states: &TWO_STATES,
        edges: &DOUBLE_OPEN_EDGES,
    };
    assert_eq!(
        verify_graph(&double_open),
        Err(GraphError::DoubleOpen { from: S1, slot: H0 })
    );

    // 6. DoubleClose.
    const DOUBLE_CLOSE_EDGES: [GraphEdge; 2] = [
        GraphEdge {
            from: S0,
            kind: EdgeKind::Ask { to: S1 },
            handle_op: HandleOp::None,
            label: "go",
        },
        GraphEdge {
            from: S1,
            kind: EdgeKind::Done,
            handle_op: HandleOp::Close(H0),
            label: "close_unopened",
        },
    ];
    let double_close = MachineGraph {
        name: "DoubleClose",
        entry: S0,
        grace_max_steps: 1,
        states: &TWO_STATES,
        edges: &DOUBLE_CLOSE_EDGES,
    };
    assert_eq!(
        verify_graph(&double_close),
        Err(GraphError::DoubleClose { from: S1, slot: H0 })
    );

    // 7. HandleMaskConflict on state merge.
    const THREE_STATES: [StateNode; 3] = [
        StateNode { id: S0, name: "S0", loop_state: false, initial_handles: 0 },
        StateNode { id: S1, name: "S1", loop_state: false, initial_handles: 0 },
        StateNode { id: S2, name: "S2", loop_state: false, initial_handles: 0 },
    ];
    const CONFLICT_EDGES: [GraphEdge; 4] = [
        GraphEdge {
            from: S0,
            kind: EdgeKind::Ask { to: S1 },
            handle_op: HandleOp::Open(H0),
            label: "branch_open",
        },
        GraphEdge {
            from: S0,
            kind: EdgeKind::Ask { to: S2 },
            handle_op: HandleOp::None,
            label: "branch_none",
        },
        GraphEdge {
            from: S1,
            kind: EdgeKind::Ask { to: S2 },
            handle_op: HandleOp::None,
            label: "merge_open",
        },
        GraphEdge {
            from: S2,
            kind: EdgeKind::Done,
            handle_op: HandleOp::None,
            label: "done",
        },
    ];
    let conflict = MachineGraph {
        name: "MaskConflict",
        entry: S0,
        grace_max_steps: 1,
        states: &THREE_STATES,
        edges: &CONFLICT_EDGES,
    };
    assert!(matches!(
        verify_graph(&conflict),
        Err(GraphError::HandleMaskConflict { state: S2, a: 0, b: 1 })
            | Err(GraphError::HandleMaskConflict { state: S2, a: 1, b: 0 })
    ));
}

// ---------------------------------------------------------------------------
// 6. Structured concurrency `Join<A, B>` + `ScopeBudget` over `Script`
// ---------------------------------------------------------------------------

enum JoinWorkMachine {
    Start { stat_id: u32, fd: u32, bytes: usize },
    Waiting,
}

impl Machine for JoinWorkMachine {
    type Req = Join<StatReq, WriteReq>;
    type Out = (u64, usize);
    type Err = DiskErr;

    fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match self {
            JoinWorkMachine::Start { stat_id, fd, bytes } => Turn::Ask {
                request: Join::new(StatReq(stat_id), WriteReq { fd, bytes }),
                next: JoinWorkMachine::Waiting,
            },
            JoinWorkMachine::Waiting => unreachable!("start called on Waiting state"),
        }
    }

    fn resume(
        self,
        answer: (u64, Result<usize, DiskErr>),
        _cx: &mut (),
    ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match answer {
            (size, Ok(written)) => Turn::Done((size, written)),
            (_, Err(err)) => Turn::Fail(err),
        }
    }
}

#[test]
fn join_request_drives_both_arms_through_script() {
    let h = heap();

    // Verify `ScopeBudget::DEFAULT` and `Join::new` / `Join::split`.
    let budget = ScopeBudget::DEFAULT;
    assert_eq!(budget.grace, GraceBudget::DEFAULT);
    assert!(budget.max_children > 0);

    let join = Join::new(StatReq(7), WriteReq { fd: 2, bytes: 64 });
    let (a, b) = join.split();
    assert_eq!(a, StatReq(7));
    assert_eq!(b, WriteReq { fd: 2, bytes: 64 });

    // Happy path: both arms complete and both answers are delivered together as a tuple.
    let mut ok_script = Script::new_in(h.alloc());
    ok_script.expect_answer(
        Join::new(StatReq(7), WriteReq { fd: 2, bytes: 64 }),
        (4096u64, Ok(64usize)),
    );
    let res = drive(
        &mut ok_script,
        &mut (),
        JoinWorkMachine::Start { stat_id: 7, fd: 2, bytes: 64 },
        DriveOpts { max_steps: 4 },
    );
    assert_eq!(res, Ok((4096, 64)));
    assert_eq!(ok_script.finish(), Ok(()));

    // Second arm fails while first succeeded: machine sees both and fails cleanly.
    let mut err_script = Script::new_in(h.alloc());
    err_script.expect_answer(
        Join::new(StatReq(7), WriteReq { fd: 2, bytes: 64 }),
        (4096u64, Err(DiskErr::NoSpace)),
    );
    let res = drive(
        &mut err_script,
        &mut (),
        JoinWorkMachine::Start { stat_id: 7, fd: 2, bytes: 64 },
        DriveOpts { max_steps: 4 },
    );
    assert_eq!(res, Err(DriveError::Machine(DiskErr::NoSpace)));
    assert_eq!(err_script.finish(), Ok(()));
}

// ---------------------------------------------------------------------------
// 7. Kernel provided-buffer ring (`BufferRing` / `LeasedBuf` / `ReturnBuf`)
// ---------------------------------------------------------------------------

enum ReturnLeaseMachine<'io> {
    Start(LeasedBuf<'io>),
    Waiting,
}

impl<'io> Machine for ReturnLeaseMachine<'io> {
    type Req = ReturnBuf<'io>;
    type Out = ();
    type Err = LeaseError;

    fn start(self, _cx: &mut ()) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match self {
            ReturnLeaseMachine::Start(lease) => Turn::Ask {
                request: lease.into_return(),
                next: ReturnLeaseMachine::Waiting,
            },
            ReturnLeaseMachine::Waiting => unreachable!("start called on Waiting state"),
        }
    }

    fn resume(
        self,
        answer: Result<(), LeaseError>,
        _cx: &mut (),
    ) -> Turn<Self, Self::Req, Self::Out, Self::Err> {
        match answer {
            Ok(()) => Turn::Done(()),
            Err(err) => Turn::Fail(err),
        }
    }
}

#[test]
fn buffer_ring_lease_acquire_return_and_planted_controls() {
    let h = heap();
    with_brand(|brand| {
        let group = RingGroupId(3);
        let mut ring = BufferRing::new_in(group, 2, brand, h.alloc());
        assert_eq!(ring.group(), group);
        assert_eq!(ring.capacity(), 2);
        assert_eq!(ring.outstanding(), 0);
        assert_eq!(ring.available(), 2);
        assert_eq!(ring.verify_drained(), Ok(()));

        // Acquire both slots from the 2-buffer ring.
        let b0 = ring.acquire_for_rx(128).expect("slot 0 available");
        let b1 = ring.acquire_for_rx(256).expect("slot 1 available");
        assert_eq!(b0.group(), group);
        assert_eq!(b0.len(), 128);
        assert!(!b0.is_empty());
        assert_eq!(b1.len(), 256);
        assert_ne!(b0.buf_id(), b1.buf_id());
        assert!(ring.is_in_flight(b0.buf_id()));
        assert!(ring.is_in_flight(b1.buf_id()));
        assert_eq!(ring.outstanding(), 2);
        assert_eq!(ring.available(), 0);

        // Third acquire refuses cleanly with `RingEmpty`.
        match ring.acquire_for_rx(64) {
            Err(LeaseError::RingEmpty) => {}
            Err(other) => panic!("expected RingEmpty, got {other:?}"),
            Ok(unexpected) => {
                let _ = ring.release(unexpected.into_return());
                panic!("expected RingEmpty, acquired unexpected lease");
            }
        }

        // Leak check catches outstanding leases; `SkipLeakCheck` planted control misses it.
        assert_eq!(ring.verify_drained(), Err(LeaseError::LeakedLeases { outstanding: 2 }));
        assert_eq!(ring.verify_drained_with(LeasePlant::SkipLeakCheck), Ok(()));

        // Return `b0` cleanly.
        let id0 = b0.buf_id();
        assert_eq!(ring.release(ReturnBuf::new(b0)), Ok(()));
        assert!(!ring.is_in_flight(id0));
        assert_eq!(ring.outstanding(), 1);
        assert_eq!(ring.verify_drained(), Err(LeaseError::LeakedLeases { outstanding: 1 }));

        // Double-return is caught by `release`, and missed by `SkipDoubleReturnCheck` control.
        let dup1 = LeasedBuf::new_for_ring(group, id0, 128, brand);
        assert_eq!(
            ring.release(ReturnBuf::new(dup1)),
            Err(LeaseError::AlreadyReturned { buf_id: id0 })
        );
        let dup2 = LeasedBuf::new_for_ring(group, id0, 128, brand);
        assert_eq!(
            ring.release_with(ReturnBuf::new(dup2), LeasePlant::SkipDoubleReturnCheck),
            Ok(())
        );

        // Out-of-range buffer id and mismatched ring group are refused with `InvalidBufId`.
        let oob = LeasedBuf::new_for_ring(group, RingBufId(2), 64, brand);
        assert_eq!(
            ring.release(ReturnBuf::new(oob)),
            Err(LeaseError::InvalidBufId { buf_id: RingBufId(2) })
        );
        let wrong_group = LeasedBuf::new_for_ring(RingGroupId(9), id0, 64, brand);
        assert_eq!(
            ring.release(ReturnBuf::new(wrong_group)),
            Err(LeaseError::InvalidBufId { buf_id: id0 })
        );

        // Re-acquire the freed slot and return it via `ReturnBuf` as a `Machine` request.
        let b2 = ring.acquire_for_rx(96).expect("returned slot is reusable");
        assert_eq!(b2.buf_id(), id0);
        assert_eq!(ring.outstanding(), 2);
        let driven = drive(
            &mut ring,
            &mut (),
            ReturnLeaseMachine::Start(b2),
            DriveOpts { max_steps: 4 },
        );
        assert_eq!(driven, Ok(()));
        assert_eq!(ring.outstanding(), 1);

        // Return the last live lease and verify the ring is completely drained.
        assert_eq!(ring.release(b1.into_return()), Ok(()));
        assert_eq!(ring.outstanding(), 0);
        assert_eq!(ring.verify_drained(), Ok(()));
    });
}

// ---------------------------------------------------------------------------
// 8. Deterministic transcript recorder (`RecordHandler` -> `Script` replay)
// ---------------------------------------------------------------------------

struct LiveTransportHandler {
    step: usize,
    cancel_at_step: Option<usize>,
}

impl Handler<TransportReq> for LiveTransportHandler {
    type Error = ();

    fn perform<Out>(
        &mut self,
        request: TransportReq,
        deliver: impl FnOnce(Perform<TransportAns>) -> Out,
    ) -> Result<Out, Self::Error> {
        let cur = self.step;
        self.step += 1;
        if self.cancel_at_step == Some(cur) {
            return Ok(deliver(Perform::Stopped(Stopped::cancelled(GraceBudget::DEFAULT))));
        }
        let ans = match request {
            TransportReq::Send { len, .. } => TransportAns::Sent(Ok(len.min(4))),
            TransportReq::RecvWithTimeout { .. } => {
                TransportAns::RecvOrTimeout(RaceOutcome::First(Ok(3)))
            }
            TransportReq::Close { .. } => TransportAns::Closed(Ok(())),
        };
        Ok(deliver(Perform::Answered(ans)))
    }
}

impl crate::RecordableRequest for TransportReq {
    type Recorded = TransportAns;

    fn record_answer(answer: &Self::Answer<'_>) -> Self::Recorded {
        answer.clone()
    }
}

#[test]
fn record_handler_replays_live_machine_as_zero_io_script() {
    let h = heap();
    let conn = ConnToken { slot: 0, generation: NonZeroU32::new(1).unwrap() };

    // 1. Record a full multi-step live run (3 partial sends + 3 partial recvs + 1 close = 7 steps).
    let mut live = LiveTransportHandler { step: 0, cancel_at_step: None };
    let mut recorder = RecordHandler::new_in(&mut live, h.alloc());
    let live_res = drive(
        &mut recorder,
        &mut (),
        HttpExchange::new(conn, 10, 8, ExchangePlant::None),
        DriveOpts { max_steps: 16 },
    );
    assert_eq!(live_res, Ok(9));
    assert_eq!(recorder.entries().len(), 7);
    assert!(matches!(
        recorder.entries()[0],
        TranscriptEntry::Answered {
            req: TransportReq::Send { offset: 0, len: 10, .. },
            ans: TransportAns::Sent(Ok(4)),
        }
    ));

    // Convert the recording into a zero-IO `Script` and replay the exact same machine.
    let mut replay = recorder.into_script(h.alloc());
    let replay_res = drive(
        &mut replay,
        &mut (),
        HttpExchange::new(conn, 10, 8, ExchangePlant::None),
        DriveOpts { max_steps: 16 },
    );
    assert_eq!(replay_res, Ok(9));
    assert_eq!(replay.finish(), Ok(()));

    // Verify copy-pasteable `Script` source emission.
    let mut src = FixedString::<4096>::new();
    assert_eq!(recorder.emit_script_source(&mut src), Ok(()));
    let text = src.text().expect("recorded script source fits in 4096 bytes");
    assert!(text.contains("let mut script = Script::new_in(alloc);"), "missing header:\n{text}");
    assert!(text.contains("script.expect_answer("), "missing expect_answer:\n{text}");
    assert!(text.contains("Send {"), "missing Send request:\n{text}");
    assert!(text.contains("RecvWithTimeout {"), "missing RecvWithTimeout request:\n{text}");
    assert!(text.contains("Close {"), "missing Close request:\n{text}");

    // 2. Also record & replay a run with an external `Stopped` cancellation + graceful `Close`.
    let mut cancel_live = LiveTransportHandler { step: 0, cancel_at_step: Some(0) };
    let mut cancel_rec = RecordHandler::new_in(&mut cancel_live, h.alloc());
    let cancel_live_res = drive(
        &mut cancel_rec,
        &mut (),
        HttpExchange::new(conn, 10, 8, ExchangePlant::None),
        DriveOpts { max_steps: 16 },
    );
    assert_eq!(cancel_live_res, Err(DriveError::Machine(NetErr::Reset)));
    assert_eq!(cancel_rec.entries().len(), 2);
    assert!(matches!(
        cancel_rec.entries()[0],
        TranscriptEntry::Stopped {
            req: TransportReq::Send { offset: 0, len: 10, .. },
            stopped,
        } if stopped == Stopped::cancelled(GraceBudget::DEFAULT)
    ));

    let mut cancel_replay = cancel_rec.into_script(h.alloc());
    let cancel_replay_res = drive(
        &mut cancel_replay,
        &mut (),
        HttpExchange::new(conn, 10, 8, ExchangePlant::None),
        DriveOpts { max_steps: 16 },
    );
    assert_eq!(cancel_replay_res, Err(DriveError::Machine(NetErr::Reset)));
    assert_eq!(cancel_replay.finish(), Ok(()));

    let mut cancel_src = FixedString::<2048>::new();
    assert_eq!(cancel_rec.emit_script_source(&mut cancel_src), Ok(()));
    let cancel_text = cancel_src.text().expect("cancel script source fits in 2048 bytes");
    assert!(cancel_text.contains("script.expect_stop("), "missing expect_stop:\n{cancel_text}");
    assert!(cancel_text.contains("Stopped::cancelled("), "missing Stopped::cancelled:\n{cancel_text}");
}
