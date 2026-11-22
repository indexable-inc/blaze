//! [`Plan`] and the postings record against a reference model of a suite, over a sequence of
//! `blaze test` requests with edits between them (rules 43, 53 and 64).
//!
//! The model: six functions and three outside reads, each with a version; five tests, each of
//! which ENTERS a drawn set of functions (one of them, its branch, decides by its version
//! whether a second set is entered too: "a read decides what else runs"), reads a drawn set of
//! reads, and panics when the versions of what it entered and read sum to 3 mod 4. A test may
//! hang (when its branch function is at version 3), read the clock (never kept), be ignored, be
//! a bench, or `should_panic` with or without a text. The simulation is the driver: it answers
//! each `Look` from a store of filed passes, runs each `Start` against the model on a virtual
//! clock, kills on `Kill`, files on `File`, and keeps the suite's head and postings between
//! requests, as the real driver does through records.
//!
//! The invariant: a test reported as a kept pass is one a run now would pass, with a pass filed
//! at the address its asks answer now; a test that ran reports what a run now gives; every test
//! let through is reported exactly once and no other; a kept pass none of whose asks moved is
//! not run again; no run outlives its deadline; at most `width` run at once; the postings bytes
//! do not depend on the order they were given in.
//!
//! Expected lines (`lanecheck.sh blaze-run test plan`):
//! `test sim::plan::a_kept_pass_is_exactly_a_pass_a_run_now_would_give ... ok`, and the three
//! `the_oracle_catches_*` controls, `judge_follows_the_reference` and
//! `postings_refuse_rows_out_of_order` `... ok`.

use blaze_own::{Alloc, Vec as Own};
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use super::{harness, key, oracle};
use crate::plan::{
    Address, Asked, Basis, Config, Effect, Exit, Failure, Filter, Ignored, Kept, Last, NotKept, Outcome, Plan, Plant, Ran,
    ShouldPanic, judge,
};
use crate::postings::{Ask, FnName, Postings, PostingsError, ReadName, encode};
use crate::{Nanos, TestId};

const FNS: usize = 6;
const READS: usize = 3;
const TESTS: usize = 5;
const PATHS: [&str; TESTS] = ["m::t0", "m::t1", "m::t2", "n::t3", "n::t4"];
const FILTERS: [&str; 3] = ["t1", "n::", "m::t2"];
const SKIP: [&str; 1] = ["t4"];
const NO_SKIP: [&str; 0] = [];
/// Every run's bound; every run that ends by itself ends well before it.
const DEADLINE: Nanos = Nanos(1_000);
const PANIC_TEXT: &str = "boom";

fn fn_name(f: usize) -> FnName {
    FnName(key(f as u64))
}

fn read_name(r: usize) -> ReadName {
    ReadName(key(100 + r as u64))
}

fn test_id(t: usize) -> TestId {
    TestId(key(200 + t as u64))
}

fn index_of(test: TestId) -> Result<usize, Broke> {
    let t = (test.0[0] as usize).wrapping_sub(200);
    match t < TESTS {
        true => Ok(t),
        false => Err(harness("an effect names a test the suite does not have")),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct World {
    fns: [u8; FNS],
    reads: [u8; READS],
}

impl World {
    fn answer(&self, ask: &Ask) -> u8 {
        match ask {
            Ask::Fn(FnName(name)) => self.fns[name[0] as usize],
            Ask::Read(ReadName(name)) => self.reads[name[0] as usize - 100],
        }
    }
}

#[derive(Clone, Copy)]
struct Spec {
    base: u8,
    branch: usize,
    alt: u8,
    reads: u8,
    hangs: bool,
    clock: bool,
    /// 0: no; 1: yes; 2: with a text the panic holds; 3: with a text it does not.
    should_panic: u64,
    ignore: bool,
    bench: bool,
}

fn spec(tape: &Tape) -> Spec {
    Spec {
        base: tape.below(64, "the functions it enters") as u8,
        branch: tape.below(FNS as u64, "its branch function") as usize,
        alt: tape.below(64, "what it enters when its branch is odd") as u8,
        reads: tape.below(8, "what it reads") as u8,
        hangs: tape.one_in(4, "it hangs when its branch is at version 3"),
        clock: tape.one_in(5, "it reads the clock"),
        should_panic: tape.below(4, "should_panic"),
        ignore: tape.one_in(6, "ignored"),
        bench: tape.one_in(8, "a bench"),
    }
}

impl Spec {
    fn should(&self) -> ShouldPanic<'static> {
        match self.should_panic {
            0 => ShouldPanic::No,
            1 => ShouldPanic::Yes,
            2 => ShouldPanic::WithText("boo"),
            _ => ShouldPanic::WithText("zz"),
        }
    }

    /// A run now: what it asks (sorted), their answers, how it ends, whether it may be kept.
    fn run(&self, w: &World) -> Fresh {
        let mut mask = self.base | (1 << self.branch);
        if w.fns[self.branch] % 2 == 1 {
            mask |= self.alt;
        }
        let mut asks: Vec<Ask> = (0..FNS).filter(|f| mask & (1 << f) != 0).map(|f| Ask::Fn(fn_name(f))).collect();
        asks.extend((0..READS).filter(|r| self.reads & (1 << r) != 0).map(|r| Ask::Read(read_name(r))));
        asks.sort();
        let answers: Vec<u8> = asks.iter().map(|ask| w.answer(ask)).collect();
        let sum: u32 = answers.iter().map(|a| u32::from(*a)).sum();
        let end = match (self.hangs && w.fns[self.branch] == 3, sum % 4 == 3) {
            (true, _) => None,
            (false, true) => Some(Exit::Panicked { text: PANIC_TEXT }),
            (false, false) => Some(Exit::Returned),
        };
        Fresh { asks, answers, end, keep: if self.clock { Err(NotKept::Clock) } else { Ok(()) } }
    }

    /// The reference: whether a run that ends by itself passes (Reference, "Testing attributes").
    fn passes(&self, exit: Exit<'_>) -> bool {
        match (self.should_panic, exit) {
            (0, Exit::Returned) => true,
            (1 | 2, Exit::Panicked { .. }) => true,
            _ => false,
        }
    }
}

struct Fresh {
    asks: Vec<Ask>,
    answers: Vec<u8>,
    /// `None`: it hangs.
    end: Option<Exit<'static>>,
    keep: Result<(), NotKept>,
}

#[derive(Clone)]
struct HeadRow {
    last: Last,
    micros: u64,
    asks: Vec<Ask>,
}

/// Addresses (each (test, asks, answers) interned to one) and the passes filed at them.
#[derive(Default)]
struct Book {
    addresses: Vec<(usize, Vec<Ask>, Vec<u8>)>,
    passes: Vec<Address>,
}

impl Book {
    fn address(&mut self, t: usize, asks: &[Ask], answers: &[u8]) -> Address {
        let found = self.addresses.iter().position(|(x, a, b)| *x == t && a.as_slice() == asks && b.as_slice() == answers);
        let index = found.unwrap_or_else(|| {
            self.addresses.push((t, asks.to_vec(), answers.to_vec()));
            self.addresses.len() - 1
        });
        Address(key(1_000 + index as u64))
    }
}

struct Running {
    test: usize,
    started: Nanos,
    deadline: Nanos,
    /// `None`: it hangs.
    ends_at: Option<Nanos>,
    exit: Exit<'static>,
    keep: Result<(), NotKept>,
    killed: bool,
}

fn request_config(tape: &Tape) -> Config<'static> {
    let ignored = match tape.below(4, "ignored mode") {
        0 | 1 => Ignored::Skip,
        2 => Ignored::Include,
        _ => Ignored::Only,
    };
    let filter = match tape.below(5, "filter") {
        0..=2 => Filter::All,
        3 => Filter::Substring(FILTERS[tape.below(FILTERS.len() as u64, "its text") as usize]),
        _ => Filter::Exact(PATHS[tape.below(TESTS as u64, "its path") as usize]),
    };
    Config {
        width: 1 + tape.below(3, "width") as u32,
        deadline: DEADLINE,
        run_all: tape.one_in(8, "run all"),
        ignored,
        benches: tape.one_in(4, "benches"),
        filter,
        skip: if tape.one_in(6, "skip") { &SKIP } else { &NO_SKIP },
    }
}

/// Whether the model lets test `t` through: the filter, `--skip` and the ignore mode.
fn let_through(config: &Config<'_>, s: &Spec, path: &str) -> bool {
    let named = match config.filter {
        Filter::All => true,
        Filter::Substring(text) => path.contains(text),
        Filter::Exact(text) => path == text,
    };
    named && !config.skip.iter().any(|text| path.contains(*text)) && (config.ignored != Ignored::Only || s.ignore)
}

fn run(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let specs: Vec<Spec> = (0..TESTS).map(|_| spec(tape)).collect();
    let mut world = World { fns: [0; FNS], reads: [0; READS] };
    let mut book = Book::default();
    let mut head: Vec<Option<HeadRow>> = vec![None; TESTS];
    for _ in 0..tape.range(1..8, "requests") {
        let before = world;
        for f in 0..FNS {
            if tape.one_in(3, "edit a function") {
                world.fns[f] = tape.below(4, "its version") as u8;
            }
        }
        for r in 0..READS {
            if tape.one_in(4, "change a read") {
                world.reads[r] = tape.below(3, "its digest") as u8;
            }
        }
        let config = request_config(tape);
        let mut moved: Vec<Ask> = (0..FNS).filter(|f| world.fns[*f] != before.fns[*f]).map(|f| Ask::Fn(fn_name(f))).collect();
        moved.extend((0..READS).filter(|r| world.reads[*r] != before.reads[*r]).map(|r| Ask::Read(read_name(r))));
        if tape.coin("moved given in reverse") {
            moved.reverse();
        }

        // The suite's postings, from the head's rows, and their bytes given in either order.
        let mut pairs: Vec<(Ask, TestId)> = Vec::new();
        for (t, row) in head.iter().enumerate() {
            if let Some(row) = row {
                pairs.extend(row.asks.iter().map(|ask| (*ask, test_id(t))));
            }
        }
        let bytes = encode(&pairs, Alloc::global()).map_err(harness)?;
        pairs.reverse();
        let again = encode(&pairs, Alloc::global()).map_err(harness)?;
        oracle(bytes.as_slice() == again.as_slice(), || "postings bytes depend on the order of their rows".to_owned())?;
        let postings = Postings::open(bytes.as_slice()).map_err(harness)?;

        let kept: Vec<Kept> = (0..TESTS)
            .filter_map(|t| head[t].as_ref().map(|row| Kept { test: test_id(t), last: row.last, micros: row.micros }))
            .collect();
        let asked: Vec<Asked<'static>> = (0..TESTS)
            .map(|t| Asked {
                test: test_id(t),
                path: PATHS[t],
                ignore: specs[t].ignore.then_some("slow"),
                should_panic: specs[t].should(),
                bench: specs[t].bench,
            })
            .collect();
        let head_before = head.clone();

        let mut plan = Plan::new(config, Alloc::global()).map_err(harness)?;
        plan.plant = plant;
        let mut out = Own::new_in(Alloc::global());
        let mut now = Nanos(0);
        plan.decide(&asked, &kept, &moved, &postings, now, &mut out).map_err(harness)?;

        let mut running: Vec<Running> = Vec::new();
        let mut reports: Vec<(usize, Outcome)> = Vec::new();
        let mut micros = [0u64; TESTS];
        for _step in 0..10_000 {
            while !out.is_empty() {
                let effects: Vec<Effect> = out.as_slice().to_vec();
                out.clear();
                for effect in effects {
                    match effect {
                        Effect::Report { test, outcome } => {
                            let t = index_of(test)?;
                            oracle(!reports.iter().any(|(r, _)| *r == t), || format!("test {t} reported twice"))?;
                            let fresh = specs[t].run(&world);
                            match outcome {
                                Outcome::Cached(Basis::Answers) => {
                                    let row = head[t].as_mut().ok_or_else(|| harness("a kept pass with no head row"))?;
                                    let answers: Vec<u8> = row.asks.iter().map(|ask| world.answer(ask)).collect();
                                    row.last = Last::Passed { address: book.address(t, &row.asks, &answers) };
                                }
                                Outcome::Passed { kept: Err(why), .. } => {
                                    head[t] = Some(HeadRow { last: Last::NotKept(why), micros: micros[t], asks: fresh.asks });
                                }
                                Outcome::Failed { .. } => {
                                    head[t] = Some(HeadRow { last: Last::Failed, micros: micros[t], asks: fresh.asks });
                                }
                                _ => {}
                            }
                            reports.push((t, outcome));
                        }
                        Effect::Look { test } => {
                            let t = index_of(test)?;
                            let row = head[t].as_ref().ok_or_else(|| harness("a look for a test with no head row"))?;
                            let answers: Vec<u8> = row.asks.iter().map(|ask| world.answer(ask)).collect();
                            let address = book.address(t, &row.asks, &answers);
                            let found = book.passes.contains(&address);
                            plan.looked(test, found, now, &mut out).map_err(harness)?;
                        }
                        Effect::Start { test, deadline } => {
                            let t = index_of(test)?;
                            oracle(running.len() < config.width as usize, || {
                                format!("test {t} started with {} running, over the width {}", running.len(), config.width)
                            })?;
                            let fresh = specs[t].run(&world);
                            let ends_at = fresh.end.map(|_| now.after(Nanos(100 + 150 * t as u64)));
                            let exit = fresh.end.unwrap_or(Exit::Returned);
                            running.push(Running { test: t, started: now, deadline, ends_at, exit, keep: fresh.keep, killed: false });
                        }
                        Effect::Kill { test } => {
                            let t = index_of(test)?;
                            let run = running.iter_mut().find(|r| r.test == t).ok_or_else(|| harness("a kill of a test not running"))?;
                            run.killed = true;
                        }
                        Effect::File { test } => {
                            let t = index_of(test)?;
                            let fresh = specs[t].run(&world);
                            let address = book.address(t, &fresh.asks, &fresh.answers);
                            if !book.passes.contains(&address) {
                                book.passes.push(address);
                            }
                            head[t] = Some(HeadRow { last: Last::Passed { address }, micros: micros[t], asks: fresh.asks });
                        }
                        Effect::Unsettle { test } => {
                            let t = index_of(test)?;
                            let row = head[t].as_mut().ok_or_else(|| harness("an unsettle of a test with no head row"))?;
                            let Last::Passed { address } = row.last else {
                                return Err(harness("an unsettle of a row that is not a kept pass"));
                            };
                            row.last = Last::Unsettled { address };
                        }
                    }
                }
            }
            if let Some(late) = running.iter().find(|r| !r.killed && r.deadline <= now) {
                let t = late.test;
                return Err(Broke::Oracle(format!("test {t} runs past its deadline and was never killed")));
            }
            if plan.done() {
                break;
            }
            if let Some(at) = running.iter().position(|r| r.killed) {
                let run = running.remove(at);
                micros[run.test] = (now.0 - run.started.0) / 1_000;
                let ran = Ran { exit: Exit::Killed, keep: run.keep, micros: micros[run.test] };
                plan.ended(test_id(run.test), ran, now, &mut out).map_err(harness)?;
                continue;
            }
            let next_end = running.iter().filter_map(|r| r.ends_at).min();
            let next = match (next_end, plan.next_deadline()) {
                (Some(end), Some(deadline)) => end.min(deadline),
                (Some(at), None) | (None, Some(at)) => at,
                (None, None) => return Err(harness("the plan is not done, and nothing runs or waits")),
            };
            now = next;
            plan.tick(now, &mut out);
            let mut at = 0;
            while at < running.len() {
                if running[at].ends_at.is_some_and(|end| end <= now) {
                    let run = running.remove(at);
                    micros[run.test] = (now.0 - run.started.0) / 1_000;
                    let ran = Ran { exit: run.exit, keep: run.keep, micros: micros[run.test] };
                    plan.ended(test_id(run.test), ran, now, &mut out).map_err(harness)?;
                } else {
                    at += 1;
                }
            }
        }
        if !plan.done() {
            return Err(harness("the plan did not end within 10,000 steps"));
        }

        // Each test let through is reported once; the rest are counted as filtered out.
        let through: Vec<usize> = (0..TESTS).filter(|t| let_through(&config, &specs[*t], PATHS[*t])).collect();
        let mut reported: Vec<usize> = reports.iter().map(|(t, _)| *t).collect();
        reported.sort_unstable();
        oracle(reported == through, || format!("reported tests {reported:?}, but the filter lets through {through:?}"))?;
        oracle(plan.filtered_out() as usize == TESTS - through.len(), || {
            format!("{} filtered out, the model says {}", plan.filtered_out(), TESTS - through.len())
        })?;

        for (t, outcome) in &reports {
            let (t, s) = (*t, &specs[*t]);
            let fresh = specs[t].run(&world);
            let now_passes = fresh.end.map(|exit| s.passes(exit));
            let not_run = match (s.bench && !config.benches, s.ignore && config.ignored == Ignored::Skip) {
                (true, _) => Some(Outcome::Bench),
                (false, true) => Some(Outcome::Ignored),
                (false, false) => None,
            };
            if let Some(want) = not_run {
                oracle(*outcome == want, || format!("test {t} reported {outcome:?}, not {want:?}"))?;
                continue;
            }
            match outcome {
                Outcome::Cached(_) => {
                    let address = book.address(t, &fresh.asks, &fresh.answers);
                    oracle(now_passes == Some(true) && fresh.keep.is_ok() && book.passes.contains(&address), || {
                        format!("test {t} cached as a pass, but a run now gives {now_passes:?} at an address with no pass filed")
                    })?;
                }
                Outcome::Passed { .. } => {
                    oracle(now_passes == Some(true), || format!("test {t} reported passed; a run now gives {now_passes:?}"))?;
                }
                Outcome::Failed { why: Failure::TimedOut { .. }, .. } => {
                    oracle(fresh.end.is_none(), || format!("test {t} timed out, but a run now ends by itself"))?;
                }
                Outcome::Failed { why, .. } => {
                    oracle(now_passes == Some(false), || format!("test {t} failed ({why:?}); a run now gives {now_passes:?}"))?;
                }
                Outcome::Ignored | Outcome::Bench => {
                    return Err(Broke::Oracle(format!("test {t} reported {outcome:?}, which it is not")));
                }
            }
            // Nothing needless: a kept pass none of whose asks moved is not run again.
            if let Some(HeadRow { last: Last::Passed { .. }, asks, .. }) = &head_before[t]
                && !config.run_all
                && !asks.iter().any(|ask| moved.contains(ask))
            {
                oracle(*outcome == Outcome::Cached(Basis::Unchanged), || {
                    format!("test {t} reported {outcome:?}; none of its asks moved, so its pass stands")
                })?;
            }
        }
    }
    Ok(())
}

/// The invariant: see the module's documentation.
#[test]
fn a_kept_pass_is_exactly_a_pass_a_run_now_would_give() {
    check(SimConfig::cases(400), |tape| run(tape, Plant::None));
}

#[test]
fn the_oracle_catches_candidates_from_the_first_moved_ask_only() {
    control(SimConfig::cases(4_000), Plant::None, Plant::FirstMovedOnly, "cached as a pass", run);
}

#[test]
fn the_oracle_catches_a_failure_kept_as_a_pass() {
    control(SimConfig::cases(4_000), Plant::None, Plant::KeepFailures, "cached as a pass", run);
}

#[test]
fn the_oracle_catches_a_run_never_killed() {
    control(SimConfig::cases(4_000), Plant::None, Plant::NoDeadline, "past its deadline", run);
}

/// The Reference, "Testing attributes": "The `should_panic` attribute makes the test only pass
/// if it actually panics", and "may optionally take an input string that must appear within the
/// panic message".
#[test]
fn judge_follows_the_reference() {
    let boom = Exit::Panicked { text: "it went boom" };
    assert_eq!(judge(ShouldPanic::No, Exit::Returned), Ok(()));
    assert_eq!(judge(ShouldPanic::No, boom), Err(Failure::Panicked));
    assert_eq!(judge(ShouldPanic::Yes, boom), Ok(()));
    assert_eq!(judge(ShouldPanic::Yes, Exit::Returned), Err(Failure::DidNotPanic));
    assert_eq!(judge(ShouldPanic::WithText("boom"), boom), Ok(()));
    assert_eq!(judge(ShouldPanic::WithText("bang"), boom), Err(Failure::PanicTextMissing));
    // A crash is not a panic, whatever the test expects.
    assert_eq!(judge(ShouldPanic::Yes, Exit::Signal { number: 11 }), Err(Failure::Signal { number: 11 }));
    assert_eq!(judge(ShouldPanic::No, Exit::Refused), Err(Failure::Refused));
}

#[test]
fn postings_refuse_rows_out_of_order() {
    let pairs = [(Ask::Fn(fn_name(1)), test_id(0)), (Ask::Read(read_name(0)), test_id(1))];
    let bytes = encode(&pairs, Alloc::global()).expect("two rows encode");
    let open = Postings::open(bytes.as_slice()).expect("written bytes open");
    let mut found = Own::new_in(Alloc::global());
    open.tests_of(Ask::Read(read_name(0)), &mut found);
    assert_eq!(found.as_slice(), &[test_id(1)]);
    // The two 72-byte rows swapped: refused at the second row, by name.
    let mut swapped = bytes.as_slice().to_vec();
    swapped[16..160].rotate_left(72);
    assert_eq!(Postings::open(&swapped).map(|p| p.len()), Err(PostingsError::Order { row: 1 }));
}
