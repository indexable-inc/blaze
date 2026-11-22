//! The zygote's pool, the child's machine, the descriptor-3 form and the OS-call classifier
//! (test.md "End state", pieces 9 to 11).
//!
//! The simulation: ten test rows, each a scripted child (a pass; a panic; an exit with no
//! report; a report cut at a drawn byte; a report whose digest was changed; a signal; a hang
//! that answers or ignores the stacks request; a pass written only after the deadline; a pass
//! whose output pipe a leftover process holds), run by the REAL [`Pool`] at a drawn width, with
//! every pipe's bytes cut into drawn pieces and every ready event (a report chunk, an output
//! chunk, a close, a reap) of every child delivered in an order the tape draws. The oracle:
//! never more than `width` children at once; each row's result arrives exactly once, with its
//! row; each is the one its script must give. Controls: [`Plant::TrustTornReport`] (the digest
//! is not checked) and [`Plant::KilledReportStands`] (a killed child's report counts) are each
//! caught.
//!
//! Expected lines (`cargo test -p blaze-run zygote`): every test `... ok`; the controls print
//! nothing of their own (blaze-sim's `control` passes when the plant is caught with the text).

use std::collections::VecDeque;

use blaze_own::{Alloc, Vec as Own};
use blaze_sim::{Broke, Config, Tape, check, control};

use super::{harness, key, oracle};
use crate::Plant;
use crate::child::{Child, ChildError, Ending, Spec, Step, read_name, seatbelt};
use crate::oscall::{Access, Allowed, Asked, Class, Clock, Kind, Observe, Op, Peer, RULES, Site, Where, classify, key as op_key, rule};
use crate::plan::{Exit, Failure, NotKept};
use crate::postings::{FnName, ReadName};
use crate::result::{Ended, OUTPUT_CAP, Place, Refusal, TEXT_CAP};
use crate::wire::{self, Frame, HEADER, Message, Outcome, Report, STACKS_CAP, WireError, encode_stacks_into};
use crate::zygote::{ChildId, Effect, ForkError, GRACE, Gate, How, Pool, PoolError, Said, Status, Stream, width};
use crate::{Nanos, TestId};

const ROWS: usize = 10;
const BUDGET: Nanos = Nanos(1_000_000_000);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Script {
    Pass,
    Panic,
    ExitNoReport { code: i32 },
    /// A whole report cut to its first `keep` bytes, then exit 0.
    Cut { keep: usize },
    /// A whole report with one digest byte changed, then exit 0.
    BadDigest,
    Signal { number: i32 },
    Hang { answers: bool },
    /// A whole pass report written half a grace after the deadline; then it waits to be killed.
    SlowPass,
    /// A pass, but something it started holds its output pipe until its group is killed.
    Leftover,
}

fn entered_of(row: u32) -> [FnName; 2] {
    [FnName(key(u64::from(row) + 1)), FnName(key(u64::from(row) + 1_000))]
}

fn message_of(row: u32) -> String {
    format!("boom in row {row}")
}

fn report_of(row: u32, panics: bool) -> Vec<u8> {
    let message = message_of(row);
    let entered = entered_of(row);
    let reads = [ReadName(key(77))];
    let outcome = match panics {
        true => Outcome::Panicked { message: &message, at: Some(Place { item: FnName(key(9)), offset: 12 }) },
        false => Outcome::Returned,
    };
    let report = Report { row, outcome, keep: Ok(()), entered: &entered, reads: &reads };
    wire::encode(&report, Alloc::global()).expect("a sim report encodes").as_slice().to_vec()
}

fn draw_script(tape: &Tape, report_len: usize) -> Script {
    match tape.below(9, "script") {
        0 => Script::Pass,
        1 => Script::Panic,
        2 => Script::ExitNoReport { code: [0, 1, 101][tape.index(3, "exit code")] },
        3 => Script::Cut { keep: tape.index(report_len, "cut at") },
        4 => Script::BadDigest,
        5 => Script::Signal { number: [6, 11][tape.index(2, "signal")] },
        6 => Script::Hang { answers: tape.coin("answers stacks") },
        7 => Script::SlowPass,
        _ => Script::Leftover,
    }
}

#[derive(Clone, Debug)]
enum Ev {
    Bytes(Vec<u8>),
    Close,
    Exit(Status),
}

/// One scripted child: three ordered queues (its report pipe, its output pipe, its reap), each
/// event with the time it is ready.
struct Sim {
    row: u32,
    pid: ChildId,
    script: Script,
    report: VecDeque<(Nanos, Ev)>,
    output: VecDeque<(Nanos, Ev)>,
    exit: VecDeque<(Nanos, Ev)>,
    ended: bool,
    report_closed: bool,
    output_closed: bool,
}

fn chunks(tape: &Tape, at: Nanos, bytes: &[u8], into: &mut VecDeque<(Nanos, Ev)>) {
    let mut from = 0;
    while from < bytes.len() {
        let len = 1 + tape.index((bytes.len() - from).min(97), "chunk");
        into.push_back((at, Ev::Bytes(bytes[from..from + len].to_vec())));
        from += len;
    }
}

impl Sim {
    fn start(tape: &Tape, row: u32, pid: ChildId, script: Script, now: Nanos) -> Sim {
        let mut sim = Sim {
            row,
            pid,
            script,
            report: VecDeque::new(),
            output: VecDeque::new(),
            exit: VecDeque::new(),
            ended: false,
            report_closed: false,
            output_closed: false,
        };
        let t = Nanos(now.0 + tape.below(BUDGET.0 / 2, "when it ends"));
        let output = vec![b'o'; tape.index(300, "output length")];
        chunks(tape, t, &output, &mut sim.output);
        let pass = report_of(row, false);
        let end = |sim: &mut Sim, at: Nanos, status: Status| {
            sim.report.push_back((at, Ev::Close));
            sim.output.push_back((at, Ev::Close));
            sim.exit.push_back((at, Ev::Exit(status)));
        };
        match script {
            Script::Pass => {
                chunks(tape, t, &pass, &mut sim.report);
                end(&mut sim, t, Status::Exited { code: 0 });
            }
            Script::Panic => {
                chunks(tape, t, &report_of(row, true), &mut sim.report);
                end(&mut sim, t, Status::Exited { code: 0 });
            }
            Script::ExitNoReport { code } => end(&mut sim, t, Status::Exited { code }),
            Script::Cut { keep } => {
                chunks(tape, t, &pass[..keep], &mut sim.report);
                end(&mut sim, t, Status::Exited { code: 0 });
            }
            Script::BadDigest => {
                let mut bad = pass.clone();
                bad[HEADER - 1] ^= 0x40;
                chunks(tape, t, &bad, &mut sim.report);
                end(&mut sim, t, Status::Exited { code: 0 });
            }
            Script::Signal { number } => end(&mut sim, t, Status::Signaled { number }),
            Script::Hang { .. } => {}
            Script::SlowPass => {
                let late = Nanos(now.0 + BUDGET.0 + GRACE.0 / 2);
                chunks(tape, late, &pass, &mut sim.report);
            }
            Script::Leftover => {
                chunks(tape, t, &pass, &mut sim.report);
                sim.report.push_back((t, Ev::Close));
                sim.exit.push_back((t, Ev::Exit(Status::Exited { code: 0 })));
            }
        }
        sim
    }

    /// The pool asked for its stacks.
    fn stacks(&mut self, now: Nanos) {
        if self.script == (Script::Hang { answers: true }) && !self.report_closed {
            let mut buffer = [0u8; STACKS_CAP];
            let len = encode_stacks_into(self.row, &[0x1000, 0x2000, 0x3000], &mut buffer).expect("stacks fit");
            self.report.push_back((Nanos(now.0 + GRACE.0 / 4), Ev::Bytes(buffer[..len].to_vec())));
        }
    }

    /// The pool killed its group: whatever is left of it ends now.
    fn kill(&mut self, now: Nanos) {
        let pending_close = |queue: &VecDeque<(Nanos, Ev)>| queue.iter().any(|(_, ev)| matches!(ev, Ev::Close));
        if !self.report_closed && !pending_close(&self.report) {
            // Nothing more of its own is written once it is dead.
            self.report.retain(|(at, _)| *at <= now);
            self.report.push_back((now, Ev::Close));
        }
        if !self.output_closed && !pending_close(&self.output) {
            self.output.retain(|(at, _)| *at <= now);
            self.output.push_back((now, Ev::Close));
        }
        if !self.ended && self.exit.is_empty() {
            self.exit.push_back((now, Ev::Exit(Status::Signaled { number: 9 })));
        }
    }

    fn earliest(&self) -> Option<Nanos> {
        [&self.report, &self.output, &self.exit].iter().filter_map(|queue| queue.front().map(|(at, _)| *at)).min()
    }
}

fn expect_ok(sim: &Sim, how: How, finished: &crate::zygote::Finished<'_>) -> Result<(), Broke> {
    let row = sim.row;
    let script = sim.script;
    let wrong = |what: &str| Broke::Oracle(format!("row {row} ({script:?}) {what}: it ended as {how:?}"));
    match (script, how) {
        (Script::Pass | Script::Leftover, How::Reported(Said::Returned)) => {
            oracle(finished.entered.as_slice() == entered_of(row).as_slice(), || format!("row {row}: entered names lost"))
        }
        (Script::Panic, How::Reported(Said::Panicked { at })) => {
            oracle(finished.text() == message_of(row) && at.is_some(), || format!("row {row}: the panic's message or place lost"))
        }
        (Script::ExitNoReport { code }, How::NoReport { code: got, torn: 0 }) if code == got => Ok(()),
        (Script::Cut { keep }, How::NoReport { code: 0, torn }) if torn == keep as u64 => Ok(()),
        (Script::BadDigest, How::NoReport { code: 0, .. }) => Ok(()),
        (Script::BadDigest, How::Reported(_)) => Err(wrong("took a torn report")),
        (Script::Signal { number }, How::Signal { number: got }) if number == got => Ok(()),
        (Script::Hang { answers }, How::TimedOut { after }) => {
            oracle(after >= BUDGET, || format!("row {row}: killed {} ns in, before its deadline", after.0))?;
            oracle(finished.stacks.is_empty() != answers, || format!("row {row}: stacks {:?}, answers {answers}", finished.stacks.as_slice()))
        }
        (Script::SlowPass, How::TimedOut { after }) => oracle(after >= BUDGET, || format!("row {row}: killed before its deadline")),
        (Script::Hang { .. } | Script::SlowPass, _) => Err(wrong("a run past its deadline counted as")),
        _ => Err(wrong("ended unlike its script")),
    }
}

fn run_with(tape: &Tape, plant: Plant, width_now: u32) -> Result<(), Broke> {
    let pass_len = report_of(0, false).len();
    let scripts: Vec<Script> = (0..ROWS).map(|_| draw_script(tape, pass_len)).collect();
    let mut pool = Pool::new(width_now, Alloc::global()).map_err(harness)?;
    pool.plant = plant;
    for row in 0..ROWS as u32 {
        pool.ask(row, TestId(key(u64::from(row) + 500)), BUDGET).map_err(harness)?;
    }
    let mut sims: Vec<Sim> = Vec::new();
    let mut seen = [false; ROWS];
    let mut now = Nanos(0);
    let mut next_pid = 100u32;
    let mut out: Own<Effect, Alloc<'static>> = Own::new_in(Alloc::global());
    for _ in 0..200_000 {
        pool.pump(&mut out);
        pool.tick(now, &mut out);
        oracle(pool.running() <= width_now as usize, || format!("{} children at width {width_now}", pool.running()))?;
        for effect in core::mem::replace(&mut out, Own::new_in(Alloc::global())).iter() {
            match *effect {
                Effect::Fork { row } => {
                    let pid = ChildId(next_pid);
                    next_pid += 1;
                    pool.forked(row, pid, now).map_err(harness)?;
                    sims.push(Sim::start(tape, row, pid, scripts[row as usize], now));
                }
                Effect::Stacks { child } => {
                    let sim = sims.iter_mut().find(|sim| sim.pid == child).ok_or_else(|| harness("stacks of no child"))?;
                    sim.stacks(now);
                }
                Effect::Kill { child } => {
                    let sim = sims.iter_mut().find(|sim| sim.pid == child).ok_or_else(|| harness("kill of no child"))?;
                    sim.kill(now);
                }
            }
        }
        while let Some(finished) = pool.next_finished() {
            let row = finished.row as usize;
            oracle(row < ROWS && !seen[row], || format!("row {row}'s result arrived twice"))?;
            seen[row] = true;
            let sim = sims.iter().find(|sim| sim.row == finished.row).ok_or_else(|| harness("a result of no child"))?;
            expect_ok(sim, finished.how, &finished)?;
        }
        if pool.idle() {
            return oracle(seen.iter().all(|seen| *seen), || format!("rows never ended: {seen:?}"));
        }
        // Every ready head of every child's three queues; the tape picks one.
        let mut ready: Vec<(usize, u8)> = Vec::new();
        for (at, sim) in sims.iter().enumerate() {
            for (which, queue) in [&sim.report, &sim.output, &sim.exit].into_iter().enumerate() {
                if queue.front().is_some_and(|(when, _)| *when <= now) {
                    ready.push((at, which as u8));
                }
            }
        }
        let Some(&(at, which)) = tape.pick(&ready, "next event") else {
            let soonest = sims.iter().filter_map(Sim::earliest).chain(pool.next_wake()).min();
            match soonest {
                Some(next) if next > now => now = next,
                Some(_) => {}
                None => return Err(harness("nothing ready, nothing due, and the pool is not idle")),
            }
            continue;
        };
        let sim = &mut sims[at];
        let pid = sim.pid;
        let (_, ev) = match which {
            0 => sim.report.pop_front(),
            1 => sim.output.pop_front(),
            _ => sim.exit.pop_front(),
        }
        .ok_or_else(|| harness("a ready queue was empty"))?;
        let stream = if which == 0 { Stream::Report } else { Stream::Output };
        match ev {
            Ev::Bytes(bytes) => pool.bytes(pid, stream, &bytes, now, &mut out).map_err(harness)?,
            Ev::Close => {
                match stream {
                    Stream::Report => sim.report_closed = true,
                    Stream::Output => sim.output_closed = true,
                }
                pool.closed(pid, stream, now).map_err(harness)?;
            }
            Ev::Exit(status) => {
                sim.ended = true;
                pool.exited(pid, status, now, &mut out).map_err(harness)?;
            }
        }
    }
    Err(harness("the pool never went idle"))
}

fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let width_now = 1 + tape.below(5, "width") as u32;
    run_with(tape, plant, width_now)
}

/// The invariant: every test row finishes once with its child's verified report digest or typed non-pass outcome, never counting a killed or corrupt child as passed.
#[test]
fn each_row_ends_once_with_the_result_its_child_gives() {
    check(Config::cases(500), |tape| run(tape, Plant::None));
}

#[test]
fn ten_tests_at_width_four_end_in_any_order_each_once() {
    check(Config::cases(300), |tape| run_with(tape, Plant::None, 4));
}

#[test]
fn the_oracle_catches_a_report_taken_without_its_digest() {
    control(Config::cases(4_000), Plant::None, Plant::TrustTornReport, "took a torn report", run);
}

#[test]
fn the_oracle_catches_a_killed_child_counted_as_passed() {
    control(Config::cases(4_000), Plant::None, Plant::KilledReportStands, "past its deadline counted as", run);
}

/// One child, its report cut at every byte, then exit 0: never a pass, always "no report" with
/// the bytes that came.
#[test]
fn a_report_cut_at_every_byte_is_no_report() {
    let whole = report_of(3, true);
    for keep in 0..whole.len() {
        let mut pool = Pool::new(1, Alloc::global()).expect("width 1");
        let mut out: Own<Effect, Alloc<'static>> = Own::new_in(Alloc::global());
        pool.ask(3, TestId(key(3)), BUDGET).expect("asked");
        pool.pump(&mut out);
        assert_eq!(out.as_slice(), &[Effect::Fork { row: 3 }]);
        let child = ChildId(7);
        pool.forked(3, child, Nanos(0)).expect("forked");
        pool.bytes(child, Stream::Report, &whole[..keep], Nanos(1), &mut out).expect("bytes");
        pool.closed(child, Stream::Report, Nanos(2)).expect("closed");
        pool.closed(child, Stream::Output, Nanos(2)).expect("closed");
        pool.exited(child, Status::Exited { code: 0 }, Nanos(3), &mut out).expect("reaped");
        let finished = pool.next_finished().expect("one result");
        assert_eq!(finished.how, How::NoReport { code: 0, torn: keep as u64 }, "cut at {keep}");
        assert_eq!(finished.exit(), Exit::NoReport { code: 0 });
        assert!(pool.idle());
    }
}

/// The plan's verdicts map onto the result record's endings, and a pair that does not fit is
/// refused by name.
#[test]
fn a_result_gives_the_plan_its_exit_and_the_record_its_ending() {
    let mut pool = Pool::new(2, Alloc::global()).expect("width 2");
    let mut out: Own<Effect, Alloc<'static>> = Own::new_in(Alloc::global());
    pool.ask(0, TestId(key(1)), BUDGET).expect("asked");
    assert_eq!(pool.ask(0, TestId(key(1)), BUDGET), Err(PoolError::TwoAsked { row: 0 }));
    pool.pump(&mut out);
    pool.forked(0, ChildId(1), Nanos(0)).expect("forked");
    pool.bytes(ChildId(1), Stream::Output, b"hello\n", Nanos(1), &mut out).expect("output");
    pool.bytes(ChildId(1), Stream::Report, &report_of(0, true), Nanos(1), &mut out).expect("report");
    pool.exited(ChildId(1), Status::Exited { code: 0 }, Nanos(2), &mut out).expect("reaped");
    pool.closed(ChildId(1), Stream::Report, Nanos(3)).expect("closed");
    pool.closed(ChildId(1), Stream::Output, Nanos(3)).expect("closed");
    let finished = pool.next_finished().expect("a result");
    assert_eq!(finished.exit(), Exit::Panicked { text: "boom in row 0" });
    let ended = finished.ended(Failure::Panicked).expect("fits");
    assert!(matches!(ended, Ended::Panicked { message: "boom in row 0", at: Some(_) }));
    assert!(finished.ended(Failure::DidNotPanic).is_err());
    let captured = finished.captured();
    assert_eq!((captured.total, captured.tail), (6, &b"hello\n"[..]));
}

#[test]
fn output_keeps_its_last_bytes_and_counts_all() {
    let mut pool = Pool::new(1, Alloc::global()).expect("width 1");
    let mut out: Own<Effect, Alloc<'static>> = Own::new_in(Alloc::global());
    pool.ask(0, TestId(key(1)), BUDGET).expect("asked");
    pool.pump(&mut out);
    pool.forked(0, ChildId(1), Nanos(0)).expect("forked");
    let mut all = Vec::new();
    for n in 0..5 * OUTPUT_CAP / 1000 {
        let piece = vec![(n % 251) as u8; 1000];
        all.extend_from_slice(&piece);
        pool.bytes(ChildId(1), Stream::Output, &piece, Nanos(1), &mut out).expect("output");
    }
    pool.closed(ChildId(1), Stream::Report, Nanos(2)).expect("closed");
    pool.closed(ChildId(1), Stream::Output, Nanos(2)).expect("closed");
    pool.exited(ChildId(1), Status::Exited { code: 3 }, Nanos(3), &mut out).expect("reaped");
    let finished = pool.next_finished().expect("a result");
    let captured = finished.captured();
    assert_eq!(captured.total, all.len() as u64);
    assert_eq!(captured.tail, &all[all.len() - OUTPUT_CAP..]);
    assert_eq!(captured.digest, blaze_blake3::hash(&all));
}

#[test]
fn the_gate_forks_only_as_one_thread() {
    let mut gate = Gate::new();
    assert!(gate.check(1).is_ok());
    assert!(gate.check(1).is_ok());
    assert_eq!(gate.check(3).map(|_| ()), Err(ForkError::Threads { count: 3, forks: 2 }));
    assert_eq!(gate.forks(), 2);
    assert_eq!(width(10, None), Ok(10));
    assert_eq!(width(0, None), Ok(1));
    assert_eq!(width(10, Some(2)), Ok(2));
    assert_eq!(width(10, Some(0)), Err(PoolError::NoWidth));
}

// The form on descriptor 3.

fn sample() -> (Vec<FnName>, Vec<ReadName>) {
    (vec![FnName(key(1)), FnName(key(2)), FnName(key(9))], vec![ReadName(key(4)), ReadName(key(5))])
}

#[test]
fn a_report_round_trips_and_every_cut_and_every_flip_is_refused() {
    let (entered, reads) = sample();
    let place = Some(Place { item: FnName(key(3)), offset: 40 });
    let outcomes = [
        Outcome::Returned,
        Outcome::Panicked { message: "it went bang ü", at: place },
        Outcome::Panicked { message: "", at: None },
        Outcome::Refused { refusal: Refusal::Write, what: "/etc/passwd" },
    ];
    for (n, outcome) in outcomes.into_iter().enumerate() {
        let keep = if n == 1 { Err(NotKept::Clock) } else { Ok(()) };
        let report = Report { row: 6, outcome, keep, entered: &entered, reads: &reads };
        let bytes = wire::encode(&report, Alloc::global()).expect("encodes");
        let bytes = bytes.as_slice();
        let Ok(Message::Ended(back)) = wire::open(bytes) else { panic!("does not open: {:?}", wire::open(bytes)) };
        assert_eq!((back.row, back.outcome, back.keep), (6, outcome, keep));
        assert_eq!(back.entered().collect::<Vec<_>>(), entered);
        assert_eq!(back.reads().collect::<Vec<_>>(), reads);
        assert_eq!(wire::frame(bytes), Ok(Frame::Whole { len: bytes.len() }));
        for cut in 0..bytes.len() {
            assert!(wire::open(&bytes[..cut]).is_err(), "a report cut at {cut} opened");
            assert!(matches!(wire::frame(&bytes[..cut]), Ok(Frame::Need { .. })), "cut at {cut}");
        }
        for at in 0..bytes.len() {
            let mut flipped = bytes.to_vec();
            flipped[at] ^= 0x01;
            assert!(wire::open(&flipped).is_err(), "a report with byte {at} changed opened");
        }
    }
}

#[test]
fn stacks_round_trip_without_allocating() {
    let mut buffer = [0u8; STACKS_CAP];
    let frames: Vec<u64> = (0..300).map(|n| 0x1_0000 + n * 4).collect();
    let len = encode_stacks_into(2, &frames, &mut buffer).expect("fits");
    let Ok(Message::Stacks(back)) = wire::open(&buffer[..len]) else { panic!("stacks do not open") };
    assert_eq!(back.row, 2);
    assert_eq!(back.frames().collect::<Vec<_>>(), frames[..wire::FRAMES_CAP].to_vec());
    let mut small = [0u8; 16];
    assert!(matches!(encode_stacks_into(2, &frames, &mut small), Err(WireError::Buffer { .. })));
}

#[test]
fn a_report_refuses_lists_out_of_order_and_lengths_over_their_caps() {
    let unsorted = [FnName(key(2)), FnName(key(1))];
    let report = Report { row: 0, outcome: Outcome::Returned, keep: Ok(()), entered: &unsorted, reads: &[] };
    assert!(matches!(wire::encode(&report, Alloc::global()), Err(WireError::Order { .. })));
    let long = "x".repeat(TEXT_CAP + 1);
    let report = Report { row: 0, outcome: Outcome::Panicked { message: &long, at: None }, keep: Ok(()), entered: &[], reads: &[] };
    assert!(matches!(wire::encode(&report, Alloc::global()), Err(WireError::TooLong { .. })));
    // A header that claims a body past the cap is refused before its body is waited for.
    let mut bytes = report_of(0, false);
    bytes[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(matches!(wire::frame(&bytes), Err(WireError::TooLong { .. })));
}

// The child.

#[test]
fn a_child_takes_its_steps_in_order_and_reports_once() {
    let env = [("HOME", "/nonexistent"), ("PATH", "/usr/bin")];
    let spec = Spec { row: 4, temp: "/tmp/blaze-test/t4", env: &env, control: None };
    let mut child = Child::new(spec, Alloc::global()).expect("a good spec");
    let mut names = Vec::new();
    while let Some(step) = child.next() {
        names.push(step.index());
        if let Step::Sandbox { profile } = step {
            assert!(profile.contains("(deny default)") && profile.contains("(subpath \"/tmp/blaze-test/t4\")"));
        }
    }
    assert_eq!(names, vec![0, 1, 2, 3, 4, 5, 7]);
    assert!(child.running());
    assert_eq!(child.saw(Op::Open { at: Where::Root("Cargo.toml"), access: Access::Read }), Allowed::Go);
    assert_eq!(child.saw(Op::Clock(Clock::Wall)), Allowed::Go);
    assert_eq!(child.saw(Op::Spawn { program: "/bin/sh" }), Allowed::Go);
    assert_eq!(child.keep(), Err(NotKept::Clock));
    let mut entered = [FnName(key(5)), FnName(key(1)), FnName(key(5))];
    let bytes = child.report(Ending::Returned, &mut entered, Alloc::global()).expect("reports");
    let Ok(Message::Ended(back)) = wire::open(bytes.as_slice()) else { panic!("the child's report does not open") };
    assert_eq!(back.entered().collect::<Vec<_>>(), vec![FnName(key(1)), FnName(key(5))]);
    assert_eq!(back.reads().collect::<Vec<_>>(), vec![read_name(&Asked::File("Cargo.toml"))]);
    assert_eq!(back.keep, Err(NotKept::Clock));
    assert!(child.report(Ending::Returned, &mut [], Alloc::global()).is_err());
}

#[test]
fn a_refusal_decides_the_outcome_and_a_bad_spec_is_refused_before_the_fork() {
    let spec = Spec { row: 1, temp: "/tmp/t1", env: &[], control: Some("/tmp/launcher.sock") };
    let mut child = Child::new(spec, Alloc::global()).expect("a good spec");
    let mut steps = Vec::new();
    while let Some(step) = child.next() {
        steps.push(step.index());
    }
    assert_eq!(steps, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(child.saw(Op::Open { at: Where::Outside("/etc/hosts"), access: Access::Write }), Allowed::Stop);
    let bytes = child.report(Ending::Panicked { message: "io error", at: None }, &mut [], Alloc::global()).expect("reports");
    let Ok(Message::Ended(back)) = wire::open(bytes.as_slice()) else { panic!("does not open") };
    assert_eq!(back.outcome, Outcome::Refused { refusal: Refusal::Write, what: "/etc/hosts" });

    let unsorted = [("PATH", "/usr/bin"), ("HOME", "/h")];
    let bad = |spec| Child::new(spec, Alloc::global()).err();
    assert_eq!(bad(Spec { row: 0, temp: "/tmp/t", env: &unsorted, control: None }), Some(ChildError::EnvOrder { at: 1 }));
    assert_eq!(bad(Spec { row: 0, temp: "/tmp/t", env: &[("TMPDIR", "/x")], control: None }), Some(ChildError::EnvTemp));
    assert_eq!(bad(Spec { row: 0, temp: "tmp/t", env: &[], control: None }), Some(ChildError::NotCanonical { path: "tmp/t" }));
    assert_eq!(bad(Spec { row: 0, temp: "/tmp/../t", env: &[], control: None }), Some(ChildError::NotCanonical { path: "/tmp/../t" }));
    assert!(matches!(seatbelt("/tmp/a\"b", None, Alloc::global()), Err(ChildError::Unsayable { .. })));
}

// The classifier.

/// Every operation form the wrappers can give, for the coverage check.
fn every_op() -> Vec<Op<'static>> {
    let places = [Where::Root("src/a.txt"), Where::Temp, Where::Constant, Where::Random, Where::Outside("/etc/hosts")];
    let mut ops = Vec::new();
    for at in places {
        ops.push(Op::Open { at, access: Access::Read });
        ops.push(Op::Open { at, access: Access::Write });
        ops.push(Op::Stat { at });
        ops.push(Op::ReadDir { at });
        ops.push(Op::Change { at });
        ops.push(Op::Connect { peer: Peer::Unix(at) });
    }
    ops.extend([
        Op::Clock(Clock::Wall),
        Op::Clock(Clock::Monotonic),
        Op::Clock(Clock::Cpu),
        Op::Env { name: "RUST_LOG" },
        Op::Random,
        Op::Connect { peer: Peer::OwnLoopback },
        Op::Connect { peer: Peer::HostLoopback },
        Op::Connect { peer: Peer::Remote("203.0.113.9:443") },
        Op::Bind { loopback: true },
        Op::Bind { loopback: false },
        Op::Spawn { program: "/bin/sh" },
        Op::UnseenDescriptor,
        Op::Foreign { symbol: "SecRandomCopyBytes" },
    ]);
    ops
}

#[test]
fn the_table_covers_every_op_once() {
    for (n, (kind, site, _)) in RULES.iter().enumerate() {
        let twice = RULES[n + 1..].iter().any(|(k, s, _)| k == kind && s == site);
        assert!(!twice, "({kind:?}, {site:?}) has two rows");
    }
    for op in every_op() {
        let (kind, site, _) = op_key(&op);
        assert!(rule(kind, site).is_some(), "{op:?} has no row");
    }
    assert_eq!(RULES.len(), every_op().len() + 1); // + (Spawn, WorkspaceBinary): only key_with with binaries gives it
}

#[test]
fn the_classifier_follows_its_table() {
    let rows: [(Op<'static>, Class<'static>); 16] = [
        (Op::Open { at: Where::Root("tests/data/x.json"), access: Access::Read }, Class::Ask(Asked::File("tests/data/x.json"))),
        (Op::Open { at: Where::Temp, access: Access::Write }, Class::Own),
        (Op::Open { at: Where::Root("out.txt"), access: Access::Write }, Class::Refused { refusal: Refusal::Write, what: "out.txt" }),
        (Op::Open { at: Where::Outside("/etc/hosts"), access: Access::Read }, Class::Ask(Asked::OutsideFile("/etc/hosts"))),
        (Op::Open { at: Where::Random, access: Access::Read }, Class::Own),
        (Op::Stat { at: Where::Root("src") }, Class::Ask(Asked::Facts("src"))),
        (Op::ReadDir { at: Where::Root("tests") }, Class::Ask(Asked::Listing("tests"))),
        (Op::Change { at: Where::Outside("/var/tmp/x") }, Class::Refused { refusal: Refusal::Write, what: "/var/tmp/x" }),
        (Op::Clock(Clock::Monotonic), Class::Own),
        (Op::Env { name: "HOME" }, Class::Ask(Asked::Var("HOME"))),
        (Op::Random, Class::Own),
        (Op::Connect { peer: Peer::OwnLoopback }, Class::Own),
        (Op::Connect { peer: Peer::HostLoopback }, Class::NotKept(NotKept::Network)),
        (Op::Connect { peer: Peer::Remote("203.0.113.9:443") }, Class::Refused { refusal: Refusal::Connect, what: "203.0.113.9:443" }),
        (Op::Spawn { program: "/usr/bin/git" }, Class::NotKept(NotKept::Foreign)),
        (Op::UnseenDescriptor, Class::NotKept(NotKept::Descriptor)),
    ];
    for (op, want) in rows {
        assert_eq!(classify(&op), want, "{op:?}");
    }
    // Sites and kinds no op gives have no row, and are refused by name, never guessed.
    assert_eq!(rule(Kind::ClockWall, Site::Root), None);
    // A file and a variable of one name are two reads.
    assert_ne!(read_name(&Asked::File("HOME")), read_name(&Asked::Var("HOME")));
}
