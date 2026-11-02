//! A deterministic simulation of record, edit and select (docs/v2/plan/lanes/fastcompile/
//! select.md, "Proof"). A program of functions, each version of which calls others (some only
//! under a bit of an input file it reads), reads or not, fails or not, and observes its own
//! position or not, is edited at random: a body edited, reverted, or moved by a line added or
//! removed above it, its code unchanged. After each edit every test is chosen for, then run for
//! the truth. A test not run must pass now, and a pass of it must have been kept at the same
//! versions and positions of what it executed and the same input. A run that ends with its
//! record incomplete keeps its pass under what the test reaches. A [`Selector`] kept across the
//! edits chooses as the store alone does.

use std::path::PathBuf;
use std::time::Duration;

use blaze_core::Traced;
use blaze_ids::{ContentId, rng::SplitMix64 as Rng};
use blaze_usercode::test::Attrs;

use crate::{
    record::Record,
    select::{
        Choice, Ran, Subject, choose, keep,
        tests::{Mem, World},
    },
    selector::Selector,
    table::{Bound, Image, Version, tests::id},
};

const FNS: u32 = 32;
/// Functions `0..SLOTS` are called through slots; the rest are in the unlogged code.
const SLOTS: u32 = 26;
const TESTS: u32 = 12;
const DATA: &str = "/ws/sim";

/// What a function does at a version: whom it calls, each under a bit of the input or always;
/// whether it reads the input; whether it fails; whether what it does depends on its own
/// position (a `#[should_panic(expected = "src/lib.rs:12")]`, an `assert_eq!(line!(), ..)`).
struct Body {
    calls: Vec<(u32, Option<u8>)>,
    reads: bool,
    fails: bool,
    observes: bool,
}

fn body(n: u32, version: u32) -> Body {
    let mut rng = Rng::new((u64::from(n) << 32) | u64::from(version));
    rng.next_u64();
    let calls: Vec<(u32, Option<u8>)> = (0..rng.below(4))
        .map(|_| {
            let callee = rng.below(u64::from(FNS)) as u32;
            let bit = (rng.below(3) == 0).then(|| rng.below(8) as u8);
            (callee, bit)
        })
        .collect();
    let reads = rng.below(4) == 0 || calls.iter().any(|it| it.1.is_some());
    let fails = rng.below(29) == 0;
    Body { calls, reads, fails, observes: rng.below(5) == 0 }
}

/// A function's bound code as codegen gives it: of its code and of the positions it names.
fn placed(n: u32, version: u32, place: u32) -> Bound {
    let bytes = [n.to_le_bytes(), version.to_le_bytes(), place.to_le_bytes()].concat();
    Bound(ContentId::of("blaze v2 testsel sim bound 1", &bytes))
}

#[derive(Clone)]
struct Program {
    versions: Vec<u32>,
    /// Each function's positions: what the span tables its code names say, which a line added
    /// above it in its file moves while its code stays.
    places: Vec<u32>,
    beside: u32,
}

struct Truth {
    /// Every function it executed, sorted.
    executed: Vec<u32>,
    read: bool,
    passed: bool,
}

impl Program {
    fn new() -> Program {
        Program { versions: vec![0; FNS as usize], places: vec![0; FNS as usize], beside: 0 }
    }

    /// A line added (or, `back`, removed) above function `from` in its file: it and the three
    /// after it move, and their code is as it was.
    fn shift(&mut self, from: usize, back: bool) {
        for place in &mut self.places[from..(from + 4).min(FNS as usize)] {
            *place = if back { place.saturating_sub(1) } else { *place + 1 };
        }
    }

    fn image(&self) -> Image {
        let fns = (0..FNS)
            .map(|n| Version { id: id(n), bound: placed(n, self.versions[n as usize], self.places[n as usize]) })
            .collect();
        Image::new(fns, (0..SLOTS).collect(), ContentId::of("blaze v2 testsel sim beside 1", &self.beside.to_le_bytes())).unwrap()
    }

    fn run(&self, root: u32, data: u8) -> Truth {
        let mut seen = vec![false; FNS as usize];
        let mut stack = vec![root];
        let (mut read, mut passed) = (false, true);
        while let Some(n) = stack.pop() {
            if std::mem::replace(&mut seen[n as usize], true) {
                continue;
            }
            let body = body(n, self.versions[n as usize]);
            read |= body.reads;
            passed &= !body.fails && !(body.observes && self.places[n as usize] % 4 == 3);
            for (callee, bit) in body.calls {
                if bit.is_none_or(|bit| (data >> bit) & 1 == 1) {
                    stack.push(callee);
                }
            }
        }
        let executed = (0..FNS).filter(|&n| seen[n as usize]).collect();
        Truth { executed, read, passed }
    }

    /// Every function a test could execute at these versions, whatever it reads.
    fn reached(&self, root: u32) -> Vec<u32> {
        let mut seen = vec![false; FNS as usize];
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            if !std::mem::replace(&mut seen[n as usize], true) {
                stack.extend(body(n, self.versions[n as usize]).calls.iter().map(|it| it.0));
            }
        }
        (0..FNS).filter(|&n| seen[n as usize]).collect()
    }
}

fn world(data: u8) -> World {
    World { files: [(PathBuf::from(DATA), [data; 32])].into(), env: Vec::new() }
}

fn traced(truth: &Truth, data: u8) -> Traced {
    match truth.read {
        true => Traced { files: vec![(PathBuf::from(DATA), Some([data; 32]))], ..Traced::default() },
        false => Traced::default(),
    }
}

fn record(truth: &Truth, whole: bool) -> Record {
    match whole {
        true => Record::of_hits(SLOTS, truth.executed.iter().copied().filter(|&n| n < SLOTS).collect()),
        false => Record::incomplete("killed"),
    }
}

/// A run, as the simulation remembers it to check a cached result against.
struct Past {
    /// Each function it executed, at its version and position then.
    executed: Vec<(u32, u32, u32)>,
    unlogged: Vec<u32>,
    unlogged_places: Vec<u32>,
    beside: u32,
    /// The input, when it read it.
    data: Option<u8>,
    passed: bool,
}

impl Past {
    fn of(program: &Program, truth: &Truth, data: u8) -> Past {
        Past {
            executed: truth
                .executed
                .iter()
                .map(|&n| (n, program.versions[n as usize], program.places[n as usize]))
                .collect(),
            unlogged: program.versions[SLOTS as usize..].to_vec(),
            unlogged_places: program.places[SLOTS as usize..].to_vec(),
            beside: program.beside,
            data: truth.read.then_some(data),
            passed: truth.passed,
        }
    }

    fn stands(&self, program: &Program, data: u8) -> bool {
        self.passed
            && self.beside == program.beside
            && self.unlogged == program.versions[SLOTS as usize..]
            && self.unlogged_places == program.places[SLOTS as usize..]
            && self.executed.iter().all(|&(n, v, p)| program.versions[n as usize] == v && program.places[n as usize] == p)
            && self.data.is_none_or(|it| it == data)
    }
}

/// A random edit: the input, the code beside, a revert, a line added or removed above a
/// function, or a body edited.
fn edit(rng: &mut Rng, program: &mut Program, data: &mut u8) {
    let n = rng.below(u64::from(FNS)) as usize;
    match rng.below(12) {
        0 => *data = rng.next_u64() as u8,
        1 => program.beside += 1,
        2 => program.versions[n] = program.versions[n].saturating_sub(1),
        3 => program.shift(n, false),
        4 => program.shift(n, true),
        _ => program.versions[n] += 1,
    }
}

fn given() -> ContentId {
    ContentId::of("blaze v2 testsel sim given 1", b"sim")
}

#[test]
fn record_edit_select_never_skips_what_would_run_otherwise() {
    let attrs = Attrs::default();
    let names: Vec<String> = (0..TESTS).map(|t| format!("t{t}")).collect();
    let (mut skipped, mut ran) = (0u32, 0u32);
    for seed in 0..24u64 {
        let mut rng = Rng::new(seed);
        let mut program = Program::new();
        let mut data = rng.next_u64() as u8;
        let mut store = Mem::default();
        let mut pasts: Vec<Vec<Past>> = (0..TESTS).map(|_| Vec::new()).collect();
        for _ in 0..60 {
            let image = program.image();
            for t in 0..TESTS {
                let root = t % SLOTS;
                let reached = program.reached(root);
                let subject = Subject { name: &names[t as usize], attrs: &attrs, outside: false, reached: &reached };
                let truth = program.run(root, data);
                let choice = choose(&store, given(), &image, &subject, false, &mut world(data)).unwrap();
                match choice {
                    Choice::Cached(_) => {
                        skipped += 1;
                        assert!(truth.passed, "seed {seed}: test {t} skipped and would fail");
                        assert!(
                            pasts[t as usize].iter().any(|it| it.stands(&program, data)),
                            "seed {seed}: test {t} skipped with no pass at what it executes now"
                        );
                    }
                    Choice::Run { .. } => {
                        ran += 1;
                        let traced = traced(&truth, data);
                        let record = record(&truth, rng.below(6) != 0);
                        let it = Ran { passed: truth.passed, took: Duration::from_millis(1), traced: Some(&traced), record: &record };
                        keep(&mut store, given(), &image, &subject, &it).unwrap();
                        pasts[t as usize].push(Past::of(&program, &truth, data));
                    }
                }
            }
            edit(&mut rng, &mut program, &mut data);
        }
    }
    assert!(skipped > ran / 4, "the simulation skips too little to say anything: {skipped} skipped, {ran} ran");
}

/// A selector kept across random edits, reverts, input changes, incomplete records, other
/// attributes, outside tests and runs of all gives every test the choice the reference walk
/// gives over the same store, every keep going through the selector; so does one made afresh
/// over the store half-way, as after a daemon restart.
#[test]
fn a_selector_chooses_as_the_store_alone_does() {
    let plain = Attrs::default();
    let ignored = Attrs { ignore: true, ..Attrs::default() };
    let names: Vec<String> = (0..TESTS).map(|t| format!("t{t}")).collect();
    let (mut cached, mut ran) = (0u32, 0u32);
    for seed in 0..16u64 {
        let mut rng = Rng::new(seed ^ 0xd1ff);
        let mut program = Program::new();
        let mut data = rng.next_u64() as u8;
        let mut store = Mem::default();
        let mut selector = Selector::new(given(), program.image());
        for step in 0..60u32 {
            match step {
                0 => {}
                30 => selector = Selector::new(given(), program.image()),
                _ => selector.advance(program.image()),
            }
            let image = program.image();
            for t in 0..TESTS {
                let root = t % SLOTS;
                let reached = program.reached(root);
                let attrs = if (t + step) % 7 == 0 { &ignored } else { &plain };
                let outside = t == 5 && step % 9 == 0;
                let subject = Subject { name: &names[t as usize], attrs, outside, reached: &reached };
                let all = step % 13 == 12;
                let want = choose(&store, given(), &image, &subject, all, &mut world(data)).unwrap();
                let got = selector.choose(&store, &subject, all, &mut world(data)).unwrap();
                assert_eq!(got, want, "seed {seed}, step {step}, test {t}");
                if let Choice::Run { .. } = got {
                    ran += 1;
                    let truth = program.run(root, data);
                    let traced = traced(&truth, data);
                    let record = record(&truth, rng.below(6) != 0);
                    let took = Duration::from_micros(1_500 + u64::from(t));
                    let it = Ran { passed: truth.passed, took, traced: Some(&traced), record: &record };
                    selector.keep(&mut store, &subject, &it).unwrap();
                } else {
                    cached += 1;
                }
            }
            edit(&mut rng, &mut program, &mut data);
        }
    }
    assert!(cached > ran / 4 && ran > 100, "{cached} cached, {ran} ran");
}

/// Over synthetic histories of one whole pass: the test runs again exactly when a function it
/// executed changed, its code or its positions, the unlogged code changed, or it read the input
/// and the input changed. A move of functions it did not execute keeps its pass.
#[test]
fn a_test_runs_exactly_when_what_it_executed_or_read_changed() {
    let attrs = Attrs::default();
    let (mut cached, mut runs) = (0u32, 0u32);
    // Of edits that only moved functions: those that kept the pass, and those that ran it.
    let (mut moved_kept, mut moved_ran) = (0u32, 0u32);
    for seed in 0..600u64 {
        let mut rng = Rng::new(seed ^ 0x5eed);
        let mut program = Program::new();
        for version in &mut program.versions {
            *version = rng.below(3) as u32;
        }
        for place in &mut program.places {
            *place = rng.below(3) as u32;
        }
        let data = rng.next_u64() as u8;
        let root = rng.below(u64::from(SLOTS)) as u32;
        let truth = program.run(root, data);
        if !truth.passed {
            continue;
        }
        let image = program.image();
        let reached = program.reached(root);
        let subject = Subject { name: "t", attrs: &attrs, outside: false, reached: &reached };
        let mut store = Mem::default();
        let traced = traced(&truth, data);
        let record = record(&truth, true);
        let ran = Ran { passed: true, took: Duration::from_millis(1), traced: Some(&traced), record: &record };
        keep(&mut store, given(), &image, &subject, &ran).unwrap();

        let before = program.clone();
        for _ in 0..1 + rng.below(3) {
            let n = rng.below(u64::from(FNS)) as usize;
            match rng.below(2) {
                0 => program.versions[n] += 1,
                _ => program.shift(n, rng.below(2) == 0),
            }
        }
        let now = if rng.below(4) == 0 { rng.next_u64() as u8 } else { data };
        let changed: Vec<u32> = (0..FNS)
            .filter(|&n| {
                let n = n as usize;
                program.versions[n] != before.versions[n] || program.places[n] != before.places[n]
            })
            .collect();
        let should_run = changed.iter().any(|n| truth.executed.binary_search(n).is_ok())
            || changed.iter().any(|&n| n >= SLOTS)
            || (truth.read && now != data);
        let choice = choose(&store, given(), &program.image(), &subject, false, &mut world(now)).unwrap();
        let runs_now = matches!(choice, Choice::Run { .. });
        assert_eq!(runs_now, should_run, "seed {seed}: executed {:?}, changed {changed:?}, {choice:?}", truth.executed);
        match runs_now {
            true => runs += 1,
            false => cached += 1,
        }
        if !changed.is_empty() && program.versions == before.versions && now == data {
            match runs_now {
                true => moved_ran += 1,
                false => moved_kept += 1,
            }
        }
    }
    assert!(cached > 50 && runs > 50, "{cached} cached, {runs} ran");
    assert!(moved_kept > 10 && moved_ran > 10, "of moves only, {moved_kept} kept the pass and {moved_ran} ran it");
}
