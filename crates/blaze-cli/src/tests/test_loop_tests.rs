//! The drive loop ([`super::TestLoop`]) on blaze's simulation (test.md, "End state", piece 16),
//! in the form of blaze-drive's sim.rs: a scripted world (a suite of ten tests over six
//! functions, a store of result records, the head and postings the loop writes and is handed
//! back next request, a runner whose runs end in an order the tape draws) and an oracle per
//! claim, then a planted-bug control per safeguard.
//!
//! Test `i`'s path is `m::t<i>`; it enters two functions ([`asks_of`]); its address is its index
//! and the versions of the functions it entered. One case is seven requests in a row:
//!
//! - (i) first run: all ten are asked;
//! - (ii) nothing moved: none is asked, the summary says `10 reused from kept results` and
//!   `test result: ok.`;
//! - (iii) one function's version moved: exactly the tests whose asks hold it are asked; one of
//!   them (drawn) panics, and its failure is printed as `FAILED m::t<k> after ...`;
//! - (iv) nothing moved: exactly that failed test is asked again;
//! - (v) it passes now, at the address its failure was filed at: one `Flaky` finding names it;
//! - (vi) another function moved and one of its tests (drawn) never ends: it is killed at the
//!   deadline and fails as `was killed past its deadline`, and the others all end;
//! - (vii) the builder cannot build test bodies: every run is answered "cannot be run yet"; the
//!   status fails, each test prints `NOT RUN m::t<k>: cannot be run yet: ...` and the summary
//!   ends `; <n> could not be run yet`; the request after asks the same tests again, and its
//!   status fails exactly when the check found an error.
//!
//! Expected, from `LANECHECK_WAIT=540 bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh
//! blaze-cli test test_loop`:
//! `test test_loop::tests::the_loop_reuses_selects_files_and_fails_honestly ... ok`,
//! `test test_loop::tests::control_a_kept_failure_is_caught ... ok`,
//! `test test_loop::tests::control_a_pass_for_a_test_not_run_is_caught ... ok`.

use blaze_own::{AVec, Alloc, Vec};
use blaze_run::plan::{Address, Config, Exit, Filter, Ignored, Ran};
use blaze_run::postings::{Ask, FnName};
use blaze_run::report::Style;
use blaze_run::result::{self, AtAddress, Captured, Slot, Verdict};
use blaze_run::{CheckoutKey, Nanos, TestId};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use super::{CannotRun, Detail, Effect, Event, LoopConfig, LoopError, Plant, Status, SuiteId, TestEnded, TestLoop};

const TESTS: usize = 10;
const FNS: usize = 6;
const DEADLINE: Nanos = Nanos(50_000_000);

fn id(i: usize) -> TestId {
    let mut bytes = [0u8; 32];
    bytes[0] = 0x40;
    bytes[1] = i as u8;
    TestId(bytes)
}

fn index_of(test: TestId) -> usize {
    test.0[1] as usize
}

fn function(f: usize) -> FnName {
    FnName([0x10 + f as u8; 32])
}

/// The two functions test `i` enters: every function is entered by some test.
fn asks_of(i: usize) -> [usize; 2] {
    [i % FNS, (i * 7 + 1) % FNS]
}

fn holders(f: usize) -> AVec<'static, usize> {
    let mut out = Vec::new_in(Alloc::global());
    for i in 0..TESTS {
        if asks_of(i).contains(&f) {
            out.push(i);
        }
    }
    out
}

fn address(i: usize, versions: &[u64; FNS]) -> Address {
    let mut bytes = [0u8; 32];
    bytes[0] = i as u8 + 1;
    for (k, f) in asks_of(i).iter().enumerate() {
        bytes[1 + k] = *f as u8;
        bytes[8 + 8 * k..16 + 8 * k].copy_from_slice(&versions[*f].to_le_bytes());
    }
    Address(bytes)
}

fn put32(out: &mut AVec<'_, u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}

/// The suite's tests record, written in blaze-scope tests_record.rs's form: a 64-byte header
/// (`BZTS`, version 1, the row count, the strings' place and length, the crate key at 32), ten
/// 64-byte rows sorted by path (the item key, the path's place and length, no expected text, no
/// ignore reason, no flags), then the paths.
fn tests_record() -> &'static [u8] {
    let g = Alloc::global();
    let mut strings: AVec<'_, u8> = Vec::new_in(g);
    let mut spans = [(0u32, 0u32); TESTS];
    for (i, span) in spans.iter_mut().enumerate() {
        let path = format!("m::t{i}");
        *span = (strings.len() as u32, path.len() as u32);
        strings.extend_from_slice(path.as_bytes());
    }
    let mut out: AVec<'_, u8> = Vec::new_in(g);
    out.extend_from_slice(b"BZTS");
    put32(&mut out, 1);
    put32(&mut out, TESTS as u32);
    put32(&mut out, (64 + 64 * TESTS) as u32);
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

/// An answer the runner owes the loop.
#[derive(Clone, Copy, Debug)]
enum Pending {
    Opened,
    Moved,
    Looked(usize),
    CannotRun(usize),
    CannotBuild(usize),
    Killed(usize),
    Checked,
}

struct World {
    versions: [u64; FNS],
    /// The versions when the head was last written: what "moved since the head" is against.
    seen: [u64; FNS],
    head: Option<AVec<'static, u8>>,
    postings: Option<AVec<'static, u8>>,
    store: AVec<'static, (Address, Slot, AVec<'static, u8>)>,
    fail: Option<usize>,
    hang: Option<usize>,
    unbuilt: Option<usize>,
    runnable: bool,
    check_errors: u32,
    requests: u8,
}

impl World {
    fn new() -> World {
        let g = Alloc::global();
        World {
            versions: [1; FNS],
            seen: [1; FNS],
            head: None,
            postings: None,
            store: Vec::new_in(g),
            fail: None,
            hang: None,
            unbuilt: None,
            runnable: true,
            check_errors: 0,
            requests: 0,
        }
    }

    /// What `address` holds now, opened from the records the loop filed.
    fn held(&self, address: Address) -> Result<AtAddress<'_>, Broke> {
        let mut at: AtAddress<'_> = AtAddress { pass: None, failure: None };
        for (filed, _, bytes) in self.store.iter() {
            if *filed != address {
                continue;
            }
            match result::open(bytes) {
                Ok(Verdict::Pass(pass)) => at.pass = Some(pass),
                Ok(Verdict::Fail(failure)) => at.failure = Some(failure),
                Err(error) => return Err(Broke::Harness(format!("a filed result record does not open: {error}"))),
            }
        }
        Ok(at)
    }
}

/// What one request did, as the runner saw it.
struct Seen {
    runs: AVec<'static, usize>,
    ended: AVec<'static, usize>,
    killed: AVec<'static, usize>,
    not_run: AVec<'static, usize>,
    flaky: AVec<'static, TestId>,
    text: AVec<'static, u8>,
    status: Status,
}

impl Seen {
    fn text(&self) -> &str {
        core::str::from_utf8(self.text.as_slice()).unwrap_or("<not UTF-8>")
    }
}

fn harness(error: LoopError) -> Broke {
    Broke::Harness(format!("the loop refused: {error}"))
}

fn sorted(values: &[usize]) -> AVec<'static, usize> {
    let mut out = Vec::from_slice_in(values, Alloc::global());
    out.sort_unstable();
    out
}

/// One `blaze test` request against `world`, every answer and every run's end in an order the
/// tape draws, at a width it draws.
fn request(tape: &Tape, world: &mut World, plant: Plant) -> Result<Seen, Broke> {
    let g = Alloc::global();
    let tests = tests_record();
    let ids: [TestId; TESTS] = core::array::from_fn(id);
    let skip: [&str; 0] = [];
    let width = 1 + tape.index(4, "width") as u32;
    let config = LoopConfig {
        plan: Config { width, deadline: DEADLINE, run_all: false, ignored: Ignored::Skip, benches: false, filter: Filter::All, skip: &skip },
        style: Style::text(80, false),
        list: false,
        run: CheckoutKey([world.requests; 32]),
        suites: 1,
    };
    world.requests += 1;
    let head = world.head.take();
    let postings = world.postings.take();
    let (mut new_head, mut new_postings) = (None, None);
    let mut seen = Seen {
        runs: Vec::new_in(g),
        ended: Vec::new_in(g),
        killed: Vec::new_in(g),
        not_run: Vec::new_in(g),
        flaky: Vec::new_in(g),
        text: Vec::new_in(g),
        status: Status { passed: 0, failed: 0, ignored: 0, not_run: 0, check_errors: 0 },
    };
    let mut status = None;
    {
        let mut machine = TestLoop::new(config, g).map_err(harness)?;
        machine.plant = plant;
        let mut out: AVec<'_, Effect<'_>> = Vec::new_in(g);
        machine.begin(&mut out).map_err(harness)?;
        let mut inbox: AVec<'_, Pending> = Vec::new_in(g);
        inbox.push(Pending::Checked);
        // (test, started, ends; `None`: it never ends by itself)
        let mut running: AVec<'_, (usize, Nanos, Option<Nanos>)> = Vec::new_in(g);
        let mut now = Nanos(0);
        let mut steps = 0;
        loop {
            steps += 1;
            if steps > 10_000 {
                return Err(Broke::Harness("the request did not end in 10,000 steps".into()));
            }
            let mut effects = core::mem::replace(&mut out, Vec::new_in(g));
            effects.reverse();
            while let Some(effect) = effects.pop() {
                match effect {
                    Effect::Open { .. } => inbox.push(Pending::Opened),
                    Effect::Moved { .. } => inbox.push(Pending::Moved),
                    Effect::Look { test, .. } => inbox.push(Pending::Looked(index_of(test))),
                    Effect::Run { test, .. } => {
                        let i = index_of(test);
                        seen.runs.push(i);
                        if !world.runnable {
                            inbox.push(Pending::CannotRun(i));
                        } else if world.unbuilt == Some(i) {
                            inbox.push(Pending::CannotBuild(i));
                        } else if world.hang == Some(i) {
                            running.push((i, now, None));
                        } else {
                            let micros = 1_000 * (1 + tape.index(5, "duration") as u64);
                            running.push((i, now, Some(now.after(Nanos(micros * 1_000)))));
                        }
                    }
                    Effect::Kill { test, .. } => {
                        let i = index_of(test);
                        let Some(at) = running.iter().position(|run| run.0 == i) else {
                            return Err(Broke::Harness(format!("a kill of m::t{i}, which is not running")));
                        };
                        running[at] = running[running.len() - 1];
                        running.pop();
                        seen.killed.push(i);
                        inbox.push(Pending::Killed(i));
                    }
                    Effect::File { address, slot, bytes, .. } => world.store.push((address, slot, bytes)),
                    Effect::Flaky { finding, .. } => seen.flaky.push(finding.flaky.test),
                    Effect::Print { text } => seen.text.extend_from_slice(&text),
                    Effect::WriteHead { bytes, .. } => new_head = Some(bytes),
                    Effect::WritePostings { bytes, .. } => new_postings = Some(bytes),
                    Effect::Done { status: done } => status = Some(done),
                }
            }
            if status.is_some() {
                break;
            }
            if !inbox.is_empty() {
                let k = tape.index(inbox.len(), "answer");
                let pending = inbox[k];
                for j in k..inbox.len() - 1 {
                    inbox[j] = inbox[j + 1];
                }
                inbox.pop();
                match pending {
                    Pending::Opened => {
                        let event = Event::Opened {
                            suite: SuiteId(0),
                            tests,
                            ids: &ids,
                            head: head.as_ref().map(|bytes| bytes.as_slice()),
                            postings: postings.as_ref().map(|bytes| bytes.as_slice()),
                        };
                        machine.step(event, now, &mut out).map_err(harness)?;
                    }
                    Pending::Moved => {
                        let mut moved: AVec<'_, Ask> = Vec::new_in(g);
                        for f in 0..FNS {
                            if world.versions[f] != world.seen[f] {
                                moved.push(Ask::Fn(function(f)));
                            }
                        }
                        machine.step(Event::Moved { suite: SuiteId(0), moved: &moved }, now, &mut out).map_err(harness)?;
                    }
                    Pending::Looked(i) => {
                        let found = world.held(address(i, &world.versions))?.pass.is_some();
                        machine.step(Event::Looked { test: id(i), found }, now, &mut out).map_err(harness)?;
                    }
                    Pending::CannotRun(i) => {
                        seen.not_run.push(i);
                        machine.step(Event::CannotRun { test: id(i), why: CannotRun::BodiesNotBuilt }, now, &mut out).map_err(harness)?;
                    }
                    Pending::CannotBuild(i) => {
                        seen.not_run.push(i);
                        let why = crate::test_drive::refusal_tests::missing_code();
                        machine.step(Event::CannotBuild { suite: SuiteId(0), test: id(i), why: &why }, now, &mut out).map_err(harness)?;
                    }
                    Pending::Killed(i) => {
                        let ended = TestEnded {
                            test: id(i),
                            ran: Ran { exit: Exit::Killed, keep: Ok(()), micros: DEADLINE.0 / 1_000 },
                            detail: Detail::NONE,
                            asks: &[],
                            reads: &[],
                            address: None,
                            held: AtAddress { pass: None, failure: None },
                            output: Captured::NONE,
                        };
                        machine.step(Event::Ended(ended), now, &mut out).map_err(harness)?;
                    }
                    Pending::Checked => {
                        machine.step(Event::Checked { errors: world.check_errors }, now, &mut out).map_err(harness)?;
                    }
                }
                continue;
            }
            let next_end = running.iter().filter_map(|run| run.2).min();
            let deadline = machine.next_deadline();
            match (next_end, deadline) {
                (Some(end), deadline) if deadline.is_none_or(|deadline| end <= deadline) => {
                    let ending: AVec<'_, usize> = {
                        let mut at: AVec<'_, usize> = Vec::new_in(g);
                        for (k, run) in running.iter().enumerate() {
                            if run.2 == Some(end) {
                                at.push(k);
                            }
                        }
                        at
                    };
                    let k = ending[tape.index(ending.len(), "which end")];
                    let (i, started, _) = running[k];
                    running[k] = running[running.len() - 1];
                    running.pop();
                    now = end;
                    seen.ended.push(i);
                    let [a, b] = asks_of(i);
                    let asks = [Ask::Fn(function(a)), Ask::Fn(function(b))];
                    let exit = match world.fail == Some(i) {
                        true => Exit::Panicked { text: "boom" },
                        false => Exit::Returned,
                    };
                    let at = address(i, &world.versions);
                    let held = world.held(at)?;
                    let ended = TestEnded {
                        test: id(i),
                        ran: Ran { exit, keep: Ok(()), micros: (end.0 - started.0) / 1_000 },
                        detail: Detail::NONE,
                        asks: &asks,
                        reads: &[],
                        address: Some(at),
                        held,
                        output: Captured::NONE,
                    };
                    machine.step(Event::Ended(ended), now, &mut out).map_err(harness)?;
                }
                (_, Some(deadline)) => {
                    now = deadline;
                    machine.step(Event::Tick, now, &mut out).map_err(harness)?;
                }
                (_, None) => return Err(Broke::Harness("the loop waits for nothing, runs nothing and gave no status".into())),
            }
        }
    }
    if let Some(bytes) = new_head {
        world.head = Some(bytes);
        world.seen = world.versions;
    } else {
        world.head = head;
    }
    world.postings = new_postings.or(postings);
    seen.status = status.unwrap_or(Status { passed: 0, failed: 0, ignored: 0, not_run: 0, check_errors: 0 });
    Ok(seen)
}

/// The tests a request asked to run are exactly `want`.
fn asked_exactly(seen: &Seen, want: &[usize], says: &str) -> Result<(), Broke> {
    let (got, want) = (sorted(&seen.runs), sorted(want));
    if got.as_slice() != want.as_slice() {
        return Err(Broke::Oracle(format!("{says}: asked {:?}, want exactly {:?}\n{}", got.as_slice(), want.as_slice(), seen.text())));
    }
    Ok(())
}

fn says(seen: &Seen, words: &str, claim: &str) -> Result<(), Broke> {
    if !seen.text().contains(words) {
        return Err(Broke::Oracle(format!("{claim}: the output does not say `{words}`:\n{}", seen.text())));
    }
    Ok(())
}

/// One case: seven requests, each claim's oracle. See the module's documentation.
fn scenario(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut world = World::new();
    let all: [usize; TESTS] = core::array::from_fn(|i| i);

    // (i) The first run asks all ten.
    let first = request(tape, &mut world, plant)?;
    asked_exactly(&first, &all, "the first run")?;
    if !first.status.passed() {
        return Err(Broke::Oracle(format!("ten passes and a clean check, yet the status is {:?}", first.status)));
    }

    // (ii) Nothing moved: none is asked; ten reused.
    let second = request(tape, &mut world, plant)?;
    asked_exactly(&second, &[], "nothing moved")?;
    says(&second, "10 reused from kept results", "nothing moved: the summary says ten reused")?;
    says(&second, "test result: ok.", "nothing moved")?;

    // (iii) One function moved: exactly its tests are asked; one of them fails.
    let f = tape.index(FNS, "the moved function");
    world.versions[f] += 1;
    let moved = holders(f);
    let bad = moved[tape.index(moved.len(), "the failing test")];
    world.fail = Some(bad);
    let third = request(tape, &mut world, plant)?;
    asked_exactly(&third, &moved, &format!("function {f} moved: the tests whose asks hold it"))?;
    says(&third, &format!("FAILED m::t{bad} after"), "a failure is printed as it ends")?;
    if third.status.passed() || third.status.failed != 1 {
        return Err(Broke::Oracle(format!("m::t{bad} failed, yet the status is {:?}", third.status)));
    }

    // (iv) Nothing moved: the failed test is asked again. (v) It passes at the same address.
    world.fail = None;
    let fourth = request(tape, &mut world, plant)?;
    asked_exactly(&fourth, &[bad], &format!("a failing test was not asked again (m::t{bad} failed and nothing moved)"))?;
    if fourth.flaky.as_slice() != [id(bad)] {
        return Err(Broke::Oracle(format!("flaky: want one finding naming m::t{bad}, got {:?}", fourth.flaky.as_slice())));
    }
    says(&fourth, &format!("FLAKY m::t{bad}: passed on checkout"), "the flaky finding is in the summary")?;

    // (vi) A test past its deadline fails naming it; the others go on.
    let g_moved = tape.index(FNS, "the second moved function");
    world.versions[g_moved] += 1;
    let moved = holders(g_moved);
    let hung = moved[tape.index(moved.len(), "the test that never ends")];
    world.hang = Some(hung);
    let fifth = request(tape, &mut world, plant)?;
    asked_exactly(&fifth, &moved, &format!("function {g_moved} moved"))?;
    if fifth.killed.as_slice() != [hung] {
        return Err(Broke::Oracle(format!("the deadline: want m::t{hung} killed alone, got {:?}", fifth.killed.as_slice())));
    }
    let mut others: AVec<'_, usize> = Vec::new_in(Alloc::global());
    for i in moved.iter() {
        if *i != hung {
            others.push(*i);
        }
    }
    if sorted(&fifth.ended).as_slice() != others.as_slice() {
        return Err(Broke::Oracle(format!("past m::t{hung}'s deadline the others did not all end: {:?}", fifth.ended.as_slice())));
    }
    says(&fifth, &format!("FAILED m::t{hung} after"), "a timed-out test is a failure")?;
    says(&fifth, "was killed past its deadline", "a timed-out test names its deadline")?;
    if fifth.status.failed != 1 {
        return Err(Broke::Oracle(format!("one test timed out, yet the status is {:?}", fifth.status)));
    }
    world.hang = None;

    // (vii) No test body can be built: every run is "cannot be run yet"; never a pass.
    world.runnable = false;
    let k = tape.index(FNS, "the function moved while nothing builds");
    world.versions[k] += 1;
    let mut wanted = holders(k);
    if !wanted.contains(&hung) {
        wanted.push(hung);
    }
    let sixth = request(tape, &mut world, plant)?;
    asked_exactly(&sixth, &wanted, "the tests asked while nothing builds")?;
    let n = wanted.len();
    let honest = !sixth.status.passed()
        && sixth.status.not_run as usize == n
        && !sixth.text().contains("test result: ok")
        && sixth.text().contains(&format!("; {n} could not be run yet"));
    if !honest {
        return Err(Broke::Oracle(format!(
            "printed a pass for a test that cannot be run yet: {n} could not run, the status is {:?}:\n{}",
            sixth.status,
            sixth.text()
        )));
    }
    for i in wanted.iter() {
        if !sixth.text().contains(&format!("NOT RUN m::t{i}: cannot be run yet: ")) {
            return Err(Broke::Oracle(format!("printed a pass for a test that cannot be run yet: no NOT RUN line for m::t{i}:\n{}", sixth.text())));
        }
    }

    // After it, the same tests are asked again; the check's errors alone fail the status.
    world.runnable = true;
    world.check_errors = u32::from(tape.bool("the check finds an error"));
    let seventh = request(tape, &mut world, plant)?;
    asked_exactly(&seventh, &wanted, "a test that could not be run was not asked again")?;
    if seventh.status.passed() != (world.check_errors == 0) {
        return Err(Broke::Oracle(format!("the check found {} errors, and the status is {:?}", world.check_errors, seventh.status)));
    }
    Ok(())
}

/// Every claim of the module's documentation holds in every case.
#[test]
fn the_loop_reuses_selects_files_and_fails_honestly() {
    check(SimConfig::cases(200), |tape| scenario(tape, Plant::None));
}

#[test]
fn a_refused_first_call_invalidates_the_previous_pass() {
    check(SimConfig::cases(32), |tape| {
        let mut world = World::new();
        let first = request(tape, &mut world, Plant::None)?;
        if !first.status.passed() {
            return Err(Broke::Oracle("initial passes were not saved".into()));
        }
        let edited = tape.index(FNS, "edit a callee");
        let dependents = holders(edited);
        let test = dependents[tape.index(dependents.len(), "refused first call")];
        world.versions[edited] += 1;
        world.unbuilt = Some(test);
        let refused = request(tape, &mut world, Plant::None)?;
        says(&refused, "1 not run (code not built)", "the build refusal is counted separately")?;
        world.unbuilt = None;
        world.versions[edited] -= 1;
        // Old result bytes are still in the store. Only the refused row lost its pass
        // and asks, so the other tests can reuse the old address when the edit is undone.
        let restored = request(tape, &mut world, Plant::None)?;
        asked_exactly(&restored, &[test], "an old pass was reused despite the refused first call")
    });
}

/// Planted: the loop hands the plan a kept FAILURE as a kept pass. Caught by (iv).
#[test]
fn control_a_kept_failure_is_caught() {
    control(SimConfig::cases(200), Plant::None, Plant::KeepFailures, "a failing test was not asked again", scenario);
}

/// Planted: the loop reports a test that could not be run as a pass. Caught by (vii).
#[test]
fn control_a_pass_for_a_test_not_run_is_caught() {
    control(SimConfig::cases(200), Plant::None, Plant::NotRunAsPass, "printed a pass for a test that cannot be run yet", scenario);
}
