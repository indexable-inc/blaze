//! THE MODEL TEST of reuse by reach (tests-stage.md, "Reuse" and "Determinism"; brief T-b, T-c).
//!
//! A generated program: functions calling functions (a call goes to a later function, so runs
//! end), some calls FOLDED (inlined: the callee's body is in its caller's code record), some
//! guarded by a static's value (a branch on data: one test reaches different code as statics
//! change), statics named by functions, and tests entering at some function. The key rule is
//! kept explicit and simple:
//!
//! - a code record's key covers its own body (its version), the statics it names, its calls'
//!   callees and guards by NAME, and the KEYS of the callees it folded;
//! - a callee called through a slot is not in its caller's key: an edit of its body moves its
//!   key alone, so the reach must list it itself;
//! - a static's datum key covers its value.
//!
//! Running a test (the model's truth) executes from its entry: a slot call is a first call (a
//! `Code` cell), a folded call runs inline (no cell of its own), each named static is sent (a
//! `Datum` cell), and the test's trace is every body version run and static value read.
//!
//! Each case runs every test, keeps its record (blaze-test-reach's `Reach` and `TestRecord`),
//! then makes one to three edits (a body, a static, or nothing). After each edit the decision
//! (`enter_stored`, asked "moved" by today's keys) must equal the oracle exactly: a test is
//! rerun iff its TRUE reach (by running the model) holds a record whose key changed, and a reused
//! test's trace now equals its kept trace (else a STALE PASS). Rerun tests are run and their
//! records replaced, as the stage would.
//!
//! Controls (rule 23): a reach that drops the statics' data, and a decision that looks only at
//! the test's own body: each caught by the oracle as a stale pass.

use std::collections::BTreeSet;
use std::format;
use std::vec::Vec as StdVec;

use blaze_own::Alloc;
use blaze_sim::{Broke, Config as SimConfig, Tape, check, control};

use crate::{Because, CellKind, Digest, Enter, ImpactIndex, MAX_READS, Out, Reach, ReachOpts, Read, Reusable, TestRecord, enter_stored};

fn alloc() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// The reach the record keeps leaves out every `Datum`: a static's edit goes unseen.
    ReachDropsStatics,
    /// The decision asks only whether the test's entry function's own code record moved.
    DecisionOwnBodyOnly,
}

#[derive(Clone, Debug)]
struct Call {
    callee: usize,
    folded: bool,
    /// Taken only when this static's value is odd.
    guard: Option<usize>,
}

#[derive(Clone, Debug)]
struct Func {
    version: u64,
    calls: StdVec<Call>,
    /// The statics it names (each guard's among them).
    names: StdVec<usize>,
}

#[derive(Clone, Debug)]
struct Program {
    funcs: StdVec<Func>,
    statics: StdVec<u64>,
    /// Each test's entry function.
    tests: StdVec<usize>,
}

fn small(i: usize) -> u8 {
    u8::try_from(i).expect("the model has under 256 of everything")
}

fn fn_symbol(f: usize) -> [u8; 32] {
    Digest::of("blaze-test-reach model fn symbol", &[small(f)]).0
}

fn static_symbol(s: usize) -> [u8; 32] {
    Digest::of("blaze-test-reach model static symbol", &[small(s)]).0
}

fn test_key(t: usize) -> Digest {
    Digest::of("blaze-test-reach model test", &[small(t)])
}

/// The key rule (module doc): own body, named statics, callees and guards by name, folded
/// callees by key.
fn code_key(p: &Program, f: usize) -> Digest {
    let func = &p.funcs[f];
    let mut bytes = StdVec::new();
    bytes.push(small(f));
    bytes.extend_from_slice(&func.version.to_le_bytes());
    for s in func.names.iter() {
        bytes.push(small(*s));
    }
    for call in func.calls.iter() {
        bytes.push(small(call.callee));
        bytes.push(u8::from(call.folded));
        bytes.push(call.guard.map_or(u8::MAX, small));
        if call.folded {
            bytes.extend_from_slice(&code_key(p, call.callee).0);
        }
    }
    Digest::of("blaze-test-reach model code key", &bytes)
}

fn datum_key(p: &Program, s: usize) -> Digest {
    let mut bytes = StdVec::new();
    bytes.push(small(s));
    bytes.extend_from_slice(&p.statics[s].to_le_bytes());
    Digest::of("blaze-test-reach model datum key", &bytes)
}

fn draw(tape: &Tape) -> Program {
    let count = 2 + tape.index(7, "functions");
    let statics = tape.index(4, "statics");
    let mut funcs = StdVec::new();
    for f in 0..count {
        let names: StdVec<usize> = (0..statics).filter(|_| tape.one_in(3, "names")).collect();
        let mut calls = StdVec::new();
        if f + 1 < count {
            for _ in 0..tape.below(4, "calls") {
                let callee = f + 1 + tape.index(count - f - 1, "callee");
                let folded = tape.one_in(4, "folded");
                let guard = match names.is_empty() || !tape.one_in(3, "guarded") {
                    true => None,
                    false => Some(names[tape.index(names.len(), "guard")]),
                };
                calls.push(Call { callee, folded, guard });
            }
        }
        funcs.push(Func { version: tape.below(4, "version"), calls, names });
    }
    let values = (0..statics).map(|_| tape.below(4, "static value")).collect();
    let tests = (0..1 + tape.index(5, "tests")).map(|_| tape.index(count, "entry")).collect();
    Program { funcs, statics: values, tests }
}

/// A test's run, by the model: its true reach and its trace.
#[derive(Default)]
struct Ran {
    reads: BTreeSet<Read>,
    trace: StdVec<u64>,
}

fn exec(p: &Program, f: usize, slot_call: bool, ran: &mut Ran) {
    if slot_call {
        ran.reads.insert(Read::code(fn_symbol(f), code_key(p, f).0));
    }
    let func = &p.funcs[f];
    ran.trace.push(1000 * f as u64 + func.version);
    for s in func.names.iter() {
        ran.reads.insert(Read::datum(static_symbol(*s), datum_key(p, *s).0));
        ran.trace.push(100_000 + 1000 * *s as u64 + p.statics[*s]);
    }
    for call in func.calls.iter() {
        if let Some(s) = call.guard
            && p.statics[s] % 2 == 0
        {
            continue;
        }
        exec(p, call.callee, !call.folded, ran);
    }
}

fn run(p: &Program, t: usize) -> Ran {
    let mut ran = Ran::default();
    exec(p, p.tests[t], true, &mut ran);
    ran
}

/// The record the stage would keep of `ran`: its reach fed in an order the tape picks.
fn record_of(tape: &Tape, t: usize, ran: &Ran, plant: Plant) -> Result<StdVec<u8>, Broke> {
    let mut arrivals: StdVec<Read> = ran.reads.iter().copied().filter(|read| plant != Plant::ReachDropsStatics || read.kind != CellKind::Datum).collect();
    tape.shuffle(&mut arrivals, "arrival");
    let mut reach = Reach::new(ReachOpts { cap: MAX_READS, alloc: alloc() }).map_err(|error| Broke::Harness(format!("{error}")))?;
    for read in arrivals {
        reach.add(read).map_err(|error| Broke::Harness(format!("{error}")))?;
    }
    let reach = reach.finish().map_err(|error| Broke::Harness(format!("{error}")))?;
    let mut bytes = blaze_own::Vec::new_in(alloc());
    TestRecord::encode(test_key(t), Out::Passed, Reusable::Yes, &reach, &mut bytes);
    Ok(bytes.as_slice().to_vec())
}

/// Today's answer to "has this cell moved": its key now against the one the record holds.
fn moved_now(p: &Program, read: &Read) -> bool {
    match read.kind {
        CellKind::Code => (0..p.funcs.len()).find(|f| fn_symbol(*f) == read.name.0).is_none_or(|f| code_key(p, f) != read.value),
        CellKind::Datum => (0..p.statics.len()).find(|s| static_symbol(*s) == read.name.0).is_none_or(|s| datum_key(p, s) != read.value),
        CellKind::File | CellKind::Env => true,
    }
}

fn histories(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let mut p = draw(tape);
    let tests = p.tests.len();
    let mut stored = StdVec::new();
    let mut kept_trace = StdVec::new();
    let mut true_reach = StdVec::new();
    for t in 0..tests {
        let ran = run(&p, t);
        stored.push(record_of(tape, t, &ran, plant)?);
        kept_trace.push(ran.trace);
        true_reach.push(ran.reads);
    }
    for step in 0..1 + tape.below(3, "edits") {
        let before = p.clone();
        match tape.below(3, "edit") {
            0 => {
                let f = tape.index(p.funcs.len(), "edited fn");
                p.funcs[f].version += 1 + tape.below(2, "by");
            }
            1 if !p.statics.is_empty() => {
                let s = tape.index(p.statics.len(), "edited static");
                p.statics[s] += 1 + tape.below(3, "by");
            }
            _ => {}
        }
        tape.note(format_args!("step {step}: {p:?}"));
        // The records whose keys the edit changed, by cell: the oracle's, from the model alone.
        let mut changed: BTreeSet<(CellKind, [u8; 32])> = BTreeSet::new();
        for f in 0..p.funcs.len() {
            if code_key(&before, f) != code_key(&p, f) {
                changed.insert((CellKind::Code, fn_symbol(f)));
            }
        }
        for s in 0..p.statics.len() {
            if datum_key(&before, s) != datum_key(&p, s) {
                changed.insert((CellKind::Datum, static_symbol(s)));
            }
        }
        for t in 0..tests {
            let expected = true_reach[t].iter().any(|read: &Read| changed.contains(&(read.kind, read.name.0)));
            let entry = fn_symbol(p.tests[t]);
            let decision = enter_stored(test_key(t), Some(stored[t].as_slice()), |read| match plant {
                Plant::DecisionOwnBodyOnly => read.kind == CellKind::Code && read.name.0 == entry && moved_now(&p, read),
                Plant::None | Plant::ReachDropsStatics => moved_now(&p, read),
            });
            let now = run(&p, t);
            match decision {
                Enter::Reused => {
                    if now.trace != kept_trace[t] {
                        return Err(Broke::Oracle(format!("stale pass: test {t} was reused at step {step}, its trace was {:?} and is now {:?}", kept_trace[t], now.trace)));
                    }
                    if expected {
                        return Err(Broke::Oracle(format!("rerun set: test {t} reaches a changed record and was reused at step {step}")));
                    }
                }
                Enter::Rerun { because } => {
                    if !expected {
                        return Err(Broke::Oracle(format!("rerun set: test {t} reran at step {step} with no changed record in its reach: {because}")));
                    }
                    match because {
                        Because::Moved(read) if changed.contains(&(read.kind, read.name.0)) => {}
                        other => return Err(Broke::Oracle(format!("because: test {t} reran for {other}, not a changed record"))),
                    }
                    stored[t] = record_of(tape, t, &now, plant)?;
                    kept_trace[t] = now.trace;
                    true_reach[t] = now.reads;
                }
            }
        }
    }
    Ok(())
}

#[test]
fn reruns_exactly_the_tests_whose_reach_moved() {
    check(SimConfig::cases(400), |tape| histories(tape, Plant::None));
}

#[test]
fn control_reach_without_statics_is_a_stale_pass() {
    control(SimConfig::cases(400), Plant::None, Plant::ReachDropsStatics, "stale pass", histories);
}

#[test]
fn control_decision_on_own_body_is_a_stale_pass() {
    control(SimConfig::cases(400), Plant::None, Plant::DecisionOwnBodyOnly, "stale pass", histories);
}

fn impact_histories(tape: &Tape) -> Result<(), Broke> {
    let mut p = draw(tape);
    let tests = p.tests.len();
    let mut stored = StdVec::new();
    for t in 0..tests {
        let ran = run(&p, t);
        stored.push(record_of(tape, t, &ran, Plant::None)?);
    }
    for step in 0..1 + tape.below(3, "edits") {
        let before = p.clone();
        match tape.below(3, "edit") {
            0 => {
                let f = tape.index(p.funcs.len(), "edited fn");
                p.funcs[f].version += 1 + tape.below(2, "by");
            }
            1 if !p.statics.is_empty() => {
                let s = tape.index(p.statics.len(), "edited static");
                p.statics[s] += 1 + tape.below(3, "by");
            }
            _ => {}
        }

        // Build `ImpactIndex` over current stored records and pass ONLY changed cells (plus
        // occasional false-positive unchanged cells with their current values to exercise early cutoff).
        let records: StdVec<(Digest, Option<&[u8]>)> = (0..tests)
            .map(|t| (test_key(t), Some(stored[t].as_slice())))
            .collect();
        let index = ImpactIndex::build(&records, alloc());

        let mut changed_cells: StdVec<(CellKind, Digest, Digest)> = StdVec::new();
        for f in 0..p.funcs.len() {
            let now_k = code_key(&p, f);
            if code_key(&before, f) != now_k || tape.one_in(4, "fp_code") {
                changed_cells.push((CellKind::Code, Digest(fn_symbol(f)), now_k));
            }
        }
        for s in 0..p.statics.len() {
            let now_k = datum_key(&p, s);
            if datum_key(&before, s) != now_k || tape.one_in(4, "fp_datum") {
                changed_cells.push((CellKind::Datum, Digest(static_symbol(s)), now_k));
            }
        }
        tape.shuffle(&mut changed_cells, "changed_order");

        let selection = index.select(&changed_cells, alloc());

        for t in 0..tests {
            let expected = enter_stored(test_key(t), Some(stored[t].as_slice()), |read| moved_now(&p, read));
            let got = selection.decision_for(t as u32);
            if got != expected {
                return Err(Broke::Oracle(format!(
                    "step {step} test {t}: ImpactIndex::select gave {got:?}, enter_stored gave {expected:?}"
                )));
            }
            if matches!(got, Enter::Rerun { .. }) {
                let now = run(&p, t);
                stored[t] = record_of(tape, t, &now, Plant::None)?;
            }
        }
    }
    Ok(())
}

#[test]
fn impact_index_matches_model_histories() {
    check(SimConfig::cases(400), impact_histories);
}
