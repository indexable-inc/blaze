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
use blaze_own::{Alloc, BrandedBump, FixedVec, FrameVec, Inline, Vec, with_brand};
use blaze_sim::{Broke, Config};

use crate::{
    Batch, BatchReady, CancelAction, CancelReason, DriveError, DriveOpts, GraceBudget, Handler,
    Machine, Perform, Race, RaceOutcome, Request, Script, ScriptError, Stopped, TaskFrame,
    TaskFramePlant, Turn, drive, run_sub,
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
