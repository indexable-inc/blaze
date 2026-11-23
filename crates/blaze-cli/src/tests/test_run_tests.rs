//! The runner ([`super::Runner`]) between the REAL drive loop (test_loop.rs `TestLoop`) and a
//! simulated zygote that runs the REAL pool (blaze-run zygote.rs `Pool`) over scripted children,
//! on blaze's simulation (test.md, "End state", pieces 9 and 16).
//!
//! A case draws a suite of one to eight tests, each child's behaviour (a pass, a panic, death by
//! signal 6, an exit with no report, a hang that ignores its stacks request), the width, every
//! latency on the zygote's channel and where the zygote's reap falls in a wake's batch, and
//! whether the zygote dies after some results. The children's reports are blaze-run wire.rs's
//! own form; every deadline is the pool's, on a virtual clock.
//!
//! The oracle, per case: no row is asked twice (a failure is never retried); with no death,
//! every test is asked once and the summary's counts are the scripts' (`ran N: P passed, F
//! failed`), each failing test's path is printed, a hang is reported killed past its deadline,
//! the status counts the failures, and the request ends within one allowance plus graces per
//! test (a hang never blocks another test); with a death, the request stops with
//! `RunError::ZygoteDied` (never a hang) and every result the zygote sent before it died was
//! handed to the loop. The control plants [`RunPlant::ExitBeforeDrain`] (the reap acted on
//! before the channel is drained) and is caught losing a result.
//!
//! Expected, from `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh
//! blaze-cli test test_run`:
//! `test test_run::tests::every_test_gets_one_verdict_and_a_hang_blocks_none ... ok`,
//! `test test_run::tests::control_a_runner_that_acts_on_the_exit_first_loses_a_result ... ok`,
//! `test test_run::tests::on_linux_every_run_is_refused_for_want_of_a_sandbox ... ok`,
//! `test test_run::tests::unbuilt_bodies_keep_the_not_run_line_per_test ... ok`.

use blaze_own::{AVec, Alloc, Vec, VecDeque};
use blaze_run::head::ReadDigest;
use blaze_run::plan::{Address, Config, Filter, Ignored};
use blaze_run::postings::{FnName, ReadName};
use blaze_run::report::Style;
use blaze_run::result::AtAddress;
use blaze_run::wire::{self, Outcome, Report};
use blaze_run::zygote::{ChildId, Effect as PoolEffect, Finished, GRACE, Pool, Status, Stream};
use blaze_run::{CheckoutKey, Nanos, TestId};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use super::{Answers, ChildEnd, Heard, Host, RunError, RunPlant, Runner, Start, Unbuilt, Zygotes};
use crate::test_loop::{Effect, Event, LoopConfig, Status as LoopStatus, SuiteId, TestLoop};

const MOST_TESTS: usize = 8;
const BUDGET: Nanos = Nanos(1_000_000_000);
/// The most steps one request may take; past it the harness says the request did not end.
const STEPS: usize = 20_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Script {
    Pass,
    Panic,
    Signal,
    NoReport,
    Hang,
}

fn draw_script(tape: &Tape) -> Script {
    [Script::Pass, Script::Panic, Script::Signal, Script::NoReport, Script::Hang][tape.index(5, "script")]
}

fn id(i: usize) -> TestId {
    let mut bytes = [0u8; 32];
    bytes[0] = 0x40;
    bytes[1] = i as u8;
    TestId(bytes)
}

fn put32(out: &mut AVec<'_, u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}

/// `n` tests `m::t0`.. in blaze-scope tests_record.rs's form (as test_loop_tests.rs writes it):
/// a 64-byte header (`BZTS`, version 1, the row count, the strings' place and length, the crate
/// key at 32), 64-byte rows sorted by path, then the paths.
fn tests_record(n: usize) -> &'static [u8] {
    let g = Alloc::global();
    let mut strings: AVec<'_, u8> = Vec::new_in(g);
    let mut spans: AVec<'_, (u32, u32)> = Vec::new_in(g);
    for i in 0..n {
        let path = format!("m::t{i}");
        spans.push((strings.len() as u32, path.len() as u32));
        strings.extend_from_slice(path.as_bytes());
    }
    let mut out: AVec<'_, u8> = Vec::new_in(g);
    out.extend_from_slice(b"BZTS");
    put32(&mut out, 1);
    put32(&mut out, n as u32);
    put32(&mut out, (64 + 64 * n) as u32);
    put32(&mut out, strings.len() as u32);
    out.resize(32, 0);
    out.extend_from_slice(&[0x7e; 32]);
    for (i, (start, len)) in spans.iter().enumerate() {
        out.extend_from_slice(&[0x50 + i as u8; 32]);
        for word in [*start, *len, 0, 0, 0, 0, 0, 0] {
            put32(&mut out, word);
        }
    }
    out.extend_from_slice(&strings);
    blaze_scope::resolve::aligned(&out, blaze_own::testing::bump())
}

/// What the zygote's channel and its reap carry.
enum Item {
    Ready,
    Ended(Finished<'static>),
    Closed,
    Exited(Status),
}

struct SimChild {
    child: ChildId,
    row: u32,
    script: Script,
    /// When it ends by itself; `None`: it hangs.
    ends: Option<Nanos>,
    /// Its group was killed at this time.
    killed: Option<Nanos>,
    gone: bool,
}

/// One zygote process, simulated: its placement, its pool over scripted children, its channel
/// (first in, first out, each message after a drawn latency) and its reap (at a drawn time,
/// independent of the channel).
struct SimZygote<'t> {
    tape: &'t Tape,
    now: Nanos,
    scripts: AVec<'static, Script>,
    /// After how many results it dies.
    dies_after: Option<usize>,
    pool: Option<Pool<'static>>,
    children: AVec<'static, SimChild>,
    next_child: u32,
    width: u32,
    placed_at: Option<Nanos>,
    placed: bool,
    channel: VecDeque<(Nanos, Item), Alloc<'static>>,
    last_sent: Nanos,
    reap: Option<(Nanos, Status)>,
    reaped: bool,
    heard: AVec<'static, Item>,
    /// How many times each row was asked.
    asks: AVec<'static, u32>,
    /// Results put on the channel.
    sent: usize,
    /// The second process starts later in the same plan batch (e.g. a cold launch).
    stagger: bool,
    dead: bool,
    told_end: bool,
    starts: u32,
}

impl<'t> SimZygote<'t> {
    fn new(tape: &'t Tape, scripts: AVec<'static, Script>, dies_after: Option<usize>) -> SimZygote<'t> {
        let g = Alloc::global();
        let mut asks = Vec::new_in(g);
        asks.resize(scripts.len(), 0);
        SimZygote {
            tape,
            now: Nanos(0),
            scripts,
            dies_after,
            pool: None,
            children: Vec::new_in(g),
            next_child: 100,
            width: 1,
            placed_at: None,
            placed: false,
            channel: VecDeque::new_in(g),
            last_sent: Nanos(0),
            reap: None,
            reaped: false,
            heard: Vec::new_in(g),
            asks,
            sent: 0,
            stagger: false,
            dead: false,
            told_end: false,
            starts: 0,
        }
    }

    fn send(&mut self, at: Nanos, item: Item) {
        let when = at.after(Nanos(self.tape.below(5_000_000, "channel latency"))).max(self.last_sent);
        self.last_sent = when;
        self.channel.push_back((when, item));
    }

    /// The process ends at `at` with `status`: its channel's end is sent, its reap drawn.
    fn exit(&mut self, at: Nanos, status: Status) {
        if self.reap.is_some() {
            return;
        }
        self.pool = None;
        self.send(at, Item::Closed);
        self.reap = Some((at.after(Nanos(self.tape.below(5_000_000, "reap after"))), status));
    }

    fn next_internal(&self) -> Option<Nanos> {
        if self.dead || self.reap.is_some() {
            return None;
        }
        let mut next = match (self.placed, self.placed_at) {
            (false, Some(at)) => Some(at),
            _ => None,
        };
        let mut take = |at: Nanos| next = Some(next.map_or(at, |next: Nanos| next.min(at)));
        for child in self.children.iter().filter(|child| !child.gone) {
            if let Some(at) = child.killed.or(child.ends) {
                take(at);
            }
        }
        if let Some(at) = self.pool.as_ref().and_then(|pool| pool.next_wake()) {
            take(at);
        }
        next
    }

    /// The zygote's own work at `t`: placement, forks, children's ends and kills, the pool's
    /// deadlines, results onto the channel, and its exit.
    fn internal(&mut self, t: Nanos) {
        if self.dead || self.reap.is_some() {
            return;
        }
        if !self.placed && self.placed_at.is_some_and(|at| at <= t) {
            self.placed = true;
            self.send(t, Item::Ready);
        }
        if !self.placed {
            return;
        }
        let g = Alloc::global();
        let mut effects: AVec<'_, PoolEffect> = Vec::new_in(g);
        let Some(pool) = self.pool.as_mut() else { return };
        for _ in 0..1_000 {
            effects.clear();
            pool.pump(&mut effects);
            pool.tick(t, &mut effects);
            let mut moved = !effects.is_empty();
            for child in self.children.iter_mut().filter(|child| !child.gone) {
                let killed = child.killed.is_some_and(|at| at <= t);
                let ends = child.ends.is_some_and(|at| at <= t);
                if !killed && !ends {
                    continue;
                }
                child.gone = true;
                moved = true;
                let line = format!("row {} says this before it ends\n", child.row);
                pool.bytes(child.child, Stream::Output, line.as_bytes(), t, &mut effects).expect("the sim's output by the pool's protocol");
                let status = match (killed, child.script) {
                    (true, _) | (false, Script::Hang) => Status::Signaled { number: 9 },
                    (false, Script::Pass | Script::Panic) => {
                        let entered = [FnName([child.row as u8 + 1; 32])];
                        let outcome = match child.script {
                            Script::Panic => Outcome::Panicked { message: "boom", at: None },
                            _ => Outcome::Returned,
                        };
                        let report = Report { row: child.row, outcome, keep: Ok(()), entered: &entered, reads: &[] };
                        let bytes = wire::encode(&report, g).expect("a sim report encodes");
                        pool.bytes(child.child, Stream::Report, bytes.as_slice(), t, &mut effects).expect("the sim's report by the pool's protocol");
                        Status::Exited { code: 0 }
                    }
                    (false, Script::Signal) => Status::Signaled { number: 6 },
                    (false, Script::NoReport) => Status::Exited { code: 1 },
                };
                pool.closed(child.child, Stream::Output, t).expect("the sim's close by the pool's protocol");
                pool.closed(child.child, Stream::Report, t).expect("the sim's close by the pool's protocol");
                pool.exited(child.child, status, t, &mut effects).expect("the sim's reap by the pool's protocol");
            }
            for effect in effects.iter() {
                match *effect {
                    PoolEffect::Fork { row } => {
                        let child = ChildId(self.next_child);
                        self.next_child += 1;
                        pool.forked(row, child, t).expect("the sim's fork by the pool's protocol");
                        let script = self.scripts[row as usize];
                        let ends = match script {
                            Script::Hang => None,
                            Script::Pass if self.stagger => Some(t.after(Nanos(BUDGET.0 * 3 / 4))),
                            _ => Some(t.after(Nanos(1_000_000 + self.tape.below(BUDGET.0 / 2, "duration")))),
                        };
                        self.children.push(SimChild { child, row, script, ends, killed: None, gone: false });
                    }
                    // The hang ignores its stacks request.
                    PoolEffect::Stacks { .. } => {}
                    PoolEffect::Kill { child } => {
                        if let Some(it) = self.children.iter_mut().find(|it| it.child == child && !it.gone) {
                            it.killed = Some(t);
                        }
                    }
                }
            }
            if !moved {
                break;
            }
        }
        let mut done: AVec<'_, Finished<'static>> = Vec::new_in(g);
        while let Some(finished) = pool.next_finished() {
            done.push(finished);
        }
        let idle = pool.idle();
        done.reverse();
        while let Some(finished) = done.pop() {
            self.send(t, Item::Ended(finished));
            self.sent += 1;
            if self.dies_after == Some(self.sent) {
                self.dead = true;
                self.exit(t, Status::Signaled { number: 11 });
                return;
            }
        }
        if self.told_end && idle {
            self.exit(t, Status::Exited { code: 0 });
        }
    }
}

impl Zygotes for SimZygote<'_> {
    fn start(&mut self, _suite: SuiteId, width: u32) -> Result<Start, RunError> {
        self.starts += 1;
        self.width = width;
        self.pool = Some(Pool::new(width, Alloc::global()).expect("a width above zero"));
        self.placed_at = Some(self.now.after(Nanos(self.tape.below(3_000_000, "placement"))));
        if self.stagger {
            self.placed_at = Some(self.now);
        }
        if self.dies_after == Some(0) {
            self.dead = true;
            let at = self.now;
            self.exit(at, Status::Exited { code: 70 });
        }
        Ok(Start::Running)
    }

    fn ask(&mut self, row: u32, test: TestId, budget: Nanos) -> Result<(), RunError> {
        if self.stagger && row == 1 {
            self.now = self.now.after(Nanos(BUDGET.0 * 3 / 4));
        }
        self.asks[row as usize] += 1;
        // A dead zygote's socket buffer takes the ask; nothing reads it.
        if let Some(pool) = self.pool.as_mut() {
            pool.ask(row, test, budget).expect("the runner asks each row once");
        }
        // The zygote reads the ask at once: a placed zygote forks it now.
        let now = self.now;
        self.internal(now);
        Ok(())
    }

    fn end(&mut self) -> Result<(), RunError> {
        self.told_end = true;
        let now = self.now;
        self.internal(now);
        Ok(())
    }

    fn kill(&mut self) -> Result<(), RunError> {
        let now = self.now;
        self.dead = true;
        self.exit(now, Status::Signaled { number: 9 });
        Ok(())
    }

    fn wait(&mut self, until: Nanos) -> Result<(), RunError> {
        for _ in 0..100_000 {
            let message = self.channel.front().map(|(at, _)| *at);
            let reap = self.reap.filter(|_| !self.reaped).map(|(at, _)| at);
            let heard = match (message, reap) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let internal = self.next_internal();
            if let Some(at) = heard.filter(|at| *at <= until && internal.is_none_or(|internal| *at <= internal)) {
                self.now = self.now.max(at);
                while self.channel.front().is_some_and(|(at, _)| *at <= self.now) {
                    if let Some((_, item)) = self.channel.pop_front() {
                        self.heard.push(item);
                    }
                }
                if let Some((_, status)) = self.reap.filter(|(at, _)| !self.reaped && *at <= self.now) {
                    self.reaped = true;
                    // The reap lands anywhere in the batch: a pipe read and a `waitpid` race.
                    let place = self.tape.index(self.heard.len() + 1, "where the reap falls");
                    self.heard.insert(place, Item::Exited(status));
                }
                return Ok(());
            }
            match internal {
                Some(at) if at <= until => {
                    self.now = self.now.max(at);
                    let now = self.now;
                    self.internal(now);
                }
                _ => {
                    self.now = self.now.max(until);
                    return Ok(());
                }
            }
        }
        Err(RunError::Seam { call: "the simulated zygote's wait did not settle in 100,000 events", errno: 0, suite: None })
    }

    fn heard_len(&self) -> usize {
        self.heard.len()
    }

    fn heard(&self, at: usize) -> Option<Heard<'_>> {
        Some(match self.heard.get(at)? {
            Item::Ready => Heard::Ready,
            Item::Ended(finished) => Heard::Ended(ChildEnd::of(finished)),
            Item::Closed => Heard::Closed,
            Item::Exited(status) => Heard::Exited(*status),
        })
    }

    fn clear(&mut self) {
        self.heard.clear();
    }

    fn now(&self) -> Nanos {
        self.now
    }

    /// The simulated zygote makes its code inside its runs: nothing parks.
    fn parked(&self, _row: u32) -> Nanos {
        Nanos(0)
    }
}

/// Every counted run's address is its test id; nothing is held there.
struct SimAnswers;

impl Answers for SimAnswers {
    fn answer<'s>(
        &'s mut self,
        _suite: SuiteId,
        test: TestId,
        _entered: &[FnName],
        reads: &[ReadName],
        digests: &mut AVec<'_, (ReadName, ReadDigest)>,
    ) -> Result<(Address, AtAddress<'s>), RunError> {
        for read in reads {
            digests.push((*read, ReadDigest([7; 32])));
        }
        Ok((Address(test.0), AtAddress::EMPTY))
    }
}

fn harness(error: impl core::fmt::Display) -> Broke {
    Broke::Harness(error.to_string())
}

fn oracle(holds: bool, why: impl FnOnce() -> String) -> Result<(), Broke> {
    match holds {
        true => Ok(()),
        false => Err(Broke::Oracle(why())),
    }
}

/// What one request did.
struct Driven {
    text: String,
    status: Option<LoopStatus>,
    stopped: Option<RunError>,
}

fn config(width: u32) -> LoopConfig<'static> {
    LoopConfig {
        plan: Config { width, deadline: BUDGET, run_all: false, ignored: Ignored::Skip, benches: false, filter: Filter::All, skip: &[] },
        style: Style::text(80, false),
        list: false,
        run: CheckoutKey([1; 32]),
        suites: 1,
    }
}

/// One `blaze test` request over one suite of `n` tests: the driver's loop, as main.rs's test
/// arm will run it (the effects that are not runs answered as a store with nothing kept would).
fn drive<Z: Zygotes>(runner: &mut Runner<'static, Z>, width: u32, n: usize) -> Result<Driven, Broke> {
    let g = Alloc::global();
    let record = tests_record(n);
    let mut ids: AVec<'static, TestId> = Vec::new_in(g);
    for i in 0..n {
        ids.push(id(i));
    }
    let ids: &'static [TestId] = ids.leak_static();
    let mut machine = TestLoop::new(config(width), g).map_err(harness)?;
    let mut out: AVec<'_, Effect<'_>> = Vec::new_in(g);
    machine.begin(&mut out).map_err(harness)?;
    machine.step(Event::Checked { errors: 0 }, Nanos(0), &mut out).map_err(harness)?;
    let mut driven = Driven { text: String::new(), status: None, stopped: None };
    let mut answers = SimAnswers;
    'request: for _ in 0..STEPS {
        let mut effects = core::mem::replace(&mut out, Vec::new_in(g));
        effects.reverse();
        let mut open = false;
        while let Some(effect) = effects.pop() {
            match &effect {
                Effect::Open { .. } => open = true,
                Effect::Run { .. } | Effect::Kill { .. } => {
                    if let Err(error) = runner.effect(&effect) {
                        driven.stopped = Some(error);
                        break 'request;
                    }
                }
                Effect::Print { text } => driven.text.push_str(core::str::from_utf8(text.as_slice()).map_err(harness)?),
                Effect::Done { status } => driven.status = Some(*status),
                Effect::Moved { .. } | Effect::Look { .. } | Effect::File { .. } | Effect::Flaky { .. } | Effect::WriteHead { .. } | Effect::WritePostings { .. } => {}
            }
        }
        if driven.status.is_some() {
            if let Err(error) = runner.finish() {
                driven.stopped = Some(error);
            }
            return Ok(driven);
        }
        if open {
            let event = Event::Opened { suite: SuiteId(0), tests: record, ids, head: None, postings: None };
            machine.step(event, runner.zygotes().now(), &mut out).map_err(harness)?;
            continue;
        }
        if !runner.busy() {
            return Err(Broke::Harness("the request waits for nothing, runs nothing and gave no status".into()));
        }
        if let Err(error) = runner.step(&mut machine, &mut answers, &mut out) {
            driven.stopped = Some(error);
            break 'request;
        }
    }
    match (&driven.status, &driven.stopped) {
        (None, None) => Err(Broke::Harness(format!("the request did not end in {STEPS} steps"))),
        _ => Ok(driven),
    }
}

fn request(tape: &Tape, plant: RunPlant) -> Result<(), Broke> {
    let g = Alloc::global();
    let n = 1 + tape.index(MOST_TESTS, "tests");
    let width = 1 + tape.index(4, "width") as u32;
    let mut scripts: AVec<'static, Script> = Vec::new_in(g);
    for _ in 0..n {
        scripts.push(draw_script(tape));
    }
    let dies_after = match tape.coin("the zygote dies") {
        true => Some(tape.index(n, "results before it dies")),
        false => None,
    };
    let failing: AVec<'static, usize> = {
        let mut rows = Vec::new_in(g);
        for (i, script) in scripts.iter().enumerate() {
            if *script != Script::Pass {
                rows.push(i);
            }
        }
        rows
    };
    let hangs = scripts.iter().any(|script| *script == Script::Hang);
    let zygote = SimZygote::new(tape, scripts, dies_after);
    let mut runner = Runner::new(zygote, &config(width).plan, Host::Macos, g);
    runner.plant = plant;
    let driven = drive(&mut runner, width, n)?;
    let zygote = runner.zygotes();
    for (row, asked) in zygote.asks.iter().enumerate() {
        oracle(*asked <= 1, || format!("row {row} was asked {asked} times: a run was retried"))?;
    }
    match (dies_after, driven.stopped) {
        (None, None) => {
            let status = driven.status.ok_or_else(|| Broke::Harness("no status".into()))?;
            let text = &driven.text;
            for (row, asked) in zygote.asks.iter().enumerate() {
                oracle(*asked == 1, || format!("row {row} was asked {asked} times; every test runs once"))?;
            }
            let (f, p) = (failing.len(), n - failing.len());
            oracle(text.contains(&format!("ran {n}: {p} passed, {f} failed")), || format!("the summary's counts are not ran {n}: {p} passed, {f} failed:\n{text}"))?;
            oracle(status.failed as usize == f, || format!("the status counts {} failures of {f}", status.failed))?;
            for row in failing.iter() {
                oracle(text.contains(&format!("m::t{row}")), || format!("the failure of m::t{row} is not printed:\n{text}"))?;
            }
            oracle(!hangs || text.contains("killed past its deadline"), || format!("a hang is not reported killed past its deadline:\n{text}"))?;
            let per_test = BUDGET.0 + 3 * GRACE.0 + 50_000_000;
            let bound = Nanos(n as u64 * per_test + 100_000_000);
            oracle(zygote.now <= bound, || format!("the request took until {} ns, past {} ns: a hang blocked other tests", zygote.now.0, bound.0))
        }
        (Some(after), Some(RunError::ZygoteDied { .. })) => {
            let answered = runner.answered() as usize;
            let sent = zygote.sent;
            oracle(answered == sent, || format!("lost a result the zygote sent before it died: {answered} answered of {sent} sent (it died after {after})"))
        }
        (Some(after), None) => Err(Broke::Oracle(format!("the zygote died after {after} results and the request ended without an error"))),
        (_, Some(error)) => Err(Broke::Oracle(format!("the request stopped: {error}"))),
    }
}

#[test]
fn every_test_gets_one_verdict_and_a_hang_blocks_none() {
    check(SimConfig::cases(500), |tape| request(tape, RunPlant::None));
}

#[test]
fn a_siblings_timeout_does_not_cut_a_later_childs_allowance() {
    // Both fly at plan time zero. One hangs from fork time zero; its sibling forks at
    // 0.75 s and returns at 1.5 s, inside its own one-second allowance.
    control(SimConfig::cases(16), RunPlant::None, RunPlant::TickBeforeSibling, "the later child lost its own allowance", |tape, plant| {
        let g = Alloc::global();
        let mut zygote = SimZygote::new(tape, Vec::from_slice_in(&[Script::Hang, Script::Pass], g), None);
        zygote.stagger = true;
        let mut runner = Runner::new(zygote, &config(2).plan, Host::Macos, g);
        runner.plant = plant;
        let driven = drive(&mut runner, 2, 2)?;
        oracle(driven.stopped.is_none() && driven.text.contains("ran 2: 1 passed, 1 failed"), || format!("the later child lost its own allowance: {:?}\n{}", driven.stopped, driven.text))
    });
}

#[test]
fn control_a_runner_that_acts_on_the_exit_first_loses_a_result() {
    control(SimConfig::cases(4_000), RunPlant::None, RunPlant::ExitBeforeDrain, "lost a result the zygote sent before it died", request);
}

/// No zygote is started on a host with no test sandbox; each test prints its `NOT RUN` line with
/// the reason, and the command fails. With the host check removed from `Runner::begin_suite`,
/// the simulated zygote starts and the first assertion fails.
#[test]
fn on_linux_every_run_is_refused_for_want_of_a_sandbox() {
    let tape = Tape::new(&[], blaze_sim::Tail::Calm, 1_000);
    let g = Alloc::global();
    let mut scripts: AVec<'static, Script> = Vec::new_in(g);
    scripts.resize(3, Script::Pass);
    let mut runner = Runner::new(SimZygote::new(&tape, scripts, None), &config(2).plan, Host::Linux, g);
    let driven = drive(&mut runner, 2, 3).expect("the request ends");
    assert_eq!(runner.zygotes().starts, 0, "no zygote starts without a sandbox");
    assert!(driven.stopped.is_none(), "{:?}", driven.stopped);
    for row in 0..3 {
        assert!(driven.text.contains(&format!("NOT RUN m::t{row}: cannot be run yet: no test sandbox is built for Linux yet")), "{}", driven.text);
    }
    assert!(driven.status.is_some_and(|status| status.not_run == 3 && !status.passed()), "{:?}", driven.status.map(|status| status.not_run));
}

/// This build's zygote source: every test keeps today's line, and nothing is asked of a zygote.
#[test]
fn unbuilt_bodies_keep_the_not_run_line_per_test() {
    let g = Alloc::global();
    let mut runner = Runner::new(Unbuilt { now: || Nanos(0) }, &config(4).plan, Host::Macos, g);
    let driven = drive(&mut runner, 4, 2).expect("the request ends");
    assert!(driven.stopped.is_none(), "{:?}", driven.stopped);
    assert!(driven.text.contains("NOT RUN m::t0: cannot be run yet: the code builder cannot build test bodies yet (test.md, piece 8)"), "{}", driven.text);
    assert!(driven.text.contains("NOT RUN m::t1: cannot be run yet: the code builder cannot build test bodies yet (test.md, piece 8)"), "{}", driven.text);
    assert!(driven.text.contains("2 could not be run yet"), "{}", driven.text);
}

/// Regression: when one `wait` returns a sibling's completion after the clock has crossed
/// another flight's `answer_due`, `Runner::step` drains already-ready completions at `now`
/// before `watch` declares `ZygoteSilent`. Control: when no completion is ready on the drain,
/// `Runner` still kills the zygote with `RunError::ZygoteSilent`.
#[test]
fn runner_drains_ready_completions_before_declaring_zygote_silent() {
    enum WaveItem {
        Ready,
        Ended(ChildEnd<'static>),
        Closed,
        Exited(Status),
    }

    struct TwoWave {
        now: Nanos,
        step: u8,
        answer_row_zero_on_drain: bool,
        batch: AVec<'static, WaveItem>,
        rows: [Option<TestId>; 2],
    }

    impl TwoWave {
        fn finished(row: u32, test: TestId) -> WaveItem {
            WaveItem::Ended(ChildEnd {
                row,
                test,
                micros: 10,
                how: blaze_run::zygote::How::Reported(blaze_run::zygote::Said::Returned),
                keep: Ok(()),
                text: "",
                entered: &[],
                reads: &[],
                output: blaze_run::result::Captured { digest: [0; 32], total: 0, tail: &[] },
            })
        }
    }

    impl Zygotes for TwoWave {
        fn start(&mut self, _suite: SuiteId, _width: u32) -> Result<Start, RunError> {
            self.batch.push(WaveItem::Ready);
            Ok(Start::Running)
        }
        fn ask(&mut self, row: u32, test: TestId, _budget: Nanos) -> Result<(), RunError> {
            self.rows[row as usize] = Some(test);
            Ok(())
        }
        fn end(&mut self) -> Result<(), RunError> {
            self.batch.push(WaveItem::Closed);
            self.batch.push(WaveItem::Exited(Status::Exited { code: 0 }));
            Ok(())
        }
        fn kill(&mut self) -> Result<(), RunError> {
            Ok(())
        }
        fn wait(&mut self, _until: Nanos) -> Result<(), RunError> {
            if !self.batch.is_empty() {
                return Ok(());
            }
            match self.step {
                0 => {
                    self.step = 1;
                    // Past BUDGET (1 s) + ANSWER_SLACK (1.4 s) = 2.4 s: row 1 arrives first.
                    self.now = Nanos(3_000_000_000);
                    if let Some(test) = self.rows[1] {
                        self.batch.push(Self::finished(1, test));
                    }
                }
                1 => {
                    self.step = 2;
                    // Immediate drain at `now`: row 0 was already ready unless this is the control.
                    if self.answer_row_zero_on_drain
                        && let Some(test) = self.rows[0]
                    {
                        self.batch.push(Self::finished(0, test));
                    }
                }
                _ => {}
            }
            Ok(())
        }
        fn heard_len(&self) -> usize {
            self.batch.len()
        }
        fn heard(&self, at: usize) -> Option<Heard<'_>> {
            Some(match self.batch.get(at)? {
                WaveItem::Ready => Heard::Ready,
                WaveItem::Ended(end) => Heard::Ended(*end),
                WaveItem::Closed => Heard::Closed,
                WaveItem::Exited(status) => Heard::Exited(*status),
            })
        }
        fn clear(&mut self) {
            self.batch.clear();
        }
        fn now(&self) -> Nanos {
            self.now
        }
        fn parked(&self, _row: u32) -> Nanos {
            Nanos(0)
        }
    }

    let g = blaze_own::testing::heap().alloc();
    // Ready completion drained before `watch`: both tests pass with zero `ZygoteSilent`.
    let drained_zygote = TwoWave { now: Nanos(0), step: 0, answer_row_zero_on_drain: true, batch: Vec::new_in(g), rows: [None, None] };
    let mut runner = Runner::new(drained_zygote, &config(2).plan, Host::Macos, g);
    let driven = drive(&mut runner, 2, 2).expect("request completes");
    assert!(driven.stopped.is_none(), "expected drain before ZygoteSilent, got {:?}", driven.stopped);
    assert!(driven.text.contains("ran 2: 2 passed, 0 failed"), "{}", driven.text);

    // Control: genuinely silent row 0 still triggers `RunError::ZygoteSilent`.
    let silent_zygote = TwoWave { now: Nanos(0), step: 0, answer_row_zero_on_drain: false, batch: Vec::new_in(g), rows: [None, None] };
    let mut runner = Runner::new(silent_zygote, &config(2).plan, Host::Macos, g);
    let driven = drive(&mut runner, 2, 2).expect("request stops cleanly");
    assert!(matches!(driven.stopped, Some(RunError::ZygoteSilent { row: 0, .. })), "expected ZygoteSilent for row 0, got {:?}", driven.stopped);
}
