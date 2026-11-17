//! The runner's proof, on a ledger written through [`Sim`]: a log of records, each appended,
//! synced and then committed by a whole-file write of how many records are committed. Its
//! start is its recovery: every committed record is in the log, whole, or it panics; what a
//! crash left past them is cut off.
//!
//! The ledger asks nothing of the runner but its seeds: it never crashes its own machine and
//! never turns a fault on. The four claims of docs/v2/plan/design/bstd-sim-test.md, "Proof":
//!
//! - (a) each planted bug ([`Plant`]) is caught within [`Seeds::DEFAULT`];
//! - (b) the seed a failure prints, given back, is the same failure and the same trace, byte
//!   for byte;
//! - (c) the correct ledger passes every default seed, with failures injected and power cut;
//! - (d) one seed run twice is one trace.
//!
//! And the causes: with crashes left out by name, the bugs only a crash shows pass
//! (test-the-cause), and leaving a kind out changes no other draw of a seed.

use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;
use crate::{Done, Driver, FileName, Tag, sim::Faults, wait::Deadline};

const RECORD: usize = 8;

/// Records a start of the ledger adds.
const BATCH: u64 = 3;

const NAME: Name<'static> = Name { package: "blaze-core", test: "sim::seeds::tests::ledger" };

/// A safeguard of the ledger's removed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// A record is committed before its bytes are synced: right until power is lost between.
    CommitBeforeSync,
    /// A sync that failed is taken as done, and its record committed.
    FailedSyncIgnored,
}

/// Why a call of the ledger's stopped.
enum Stop {
    PowerLost,
    Failed(String),
}

/// The ledger's machine: a simulation, called one effect at a time.
struct Machine {
    sim: Sim,
    next: u64,
    plant: Plant,
}

impl Machine {
    /// `effect` handed to the simulation and waited for. A power cut ends it: the machine is
    /// crashed here, as [`World::run`] asks.
    fn call(&mut self, effect: impl FnOnce(Tag) -> Effect) -> Result<Done, Stop> {
        let tag = Tag(self.next);
        self.next += 1;
        self.sim.submit(&mut vec![effect(tag)]);
        if self.sim.cut_due() {
            self.sim.crash();
            return Err(Stop::PowerLost);
        }
        let mut told = Vec::new();
        let until = Deadline::after(self.sim.now(), Duration::from_secs(3600));
        // Bounded: one effect is in flight, and it is told within the simulation's latency.
        for _ in 0..64 {
            self.sim.poll(&mut told, until);
            if let Some(at) = told.iter().position(|it| it.tag == tag) {
                return match told.swap_remove(at).done {
                    Done::Failed(error) => Err(Stop::Failed(error.0)),
                    done => Ok(done),
                };
            }
        }
        panic!("the simulation told nothing of {tag:?} in 64 polls");
    }
}

fn name(raw: &str) -> FileName {
    FileName::new(raw).expect("a test's file name")
}

/// One start of the ledger: recovery, then [`BATCH`] more records.
fn ledger(machine: &mut Machine) -> Result<(), Stop> {
    let (log, commit) = (name("ledger/log"), name("ledger/commit"));
    let read = |machine: &mut Machine, file: &FileName| -> Result<Vec<u8>, Stop> {
        // clone: the effect's name, a refcount.
        match machine.call(|tag| Effect::ReadAll { file: file.clone(), tag })? {
            Done::Read(bytes) => Ok(bytes),
            other => panic!("a read of {file} ended {other:?}"),
        }
    };
    // Recovery. The oracle: every record the commit counts is in the log, whole.
    let committed = match read(machine, &commit)?.as_slice() {
        [] => 0,
        bytes => u64::from_le_bytes(bytes.try_into().expect("the commit record is whole or not there")),
    };
    let held = read(machine, &log)?;
    for record in 0..committed {
        let at = record as usize * RECORD;
        assert!(
            held.get(at..at + RECORD) == Some(&[record as u8 + 1; RECORD]),
            "record {record} of the {committed} committed is lost or torn: the log holds {} bytes",
            held.len()
        );
    }
    let kept = committed * RECORD as u64;
    if held.len() as u64 > kept {
        // clone: the effect's name, a refcount.
        machine.call(|tag| Effect::Truncate { file: log.clone(), len: kept, tag })?;
    }
    for record in committed..committed + BATCH {
        let bytes = vec![record as u8 + 1; RECORD];
        // clone: the effect's name, a refcount.
        machine.call(|tag| Effect::Append { file: log.clone(), bytes, tag })?;
        // clone: the effects' names, refcounts.
        let sync = |machine: &mut Machine| machine.call(|tag| Effect::Sync { file: log.clone(), tag });
        let count = |machine: &mut Machine| {
            let bytes = (record + 1).to_le_bytes().to_vec();
            machine.call(|tag| Effect::Write { file: commit.clone(), bytes, tag })
        };
        match machine.plant {
            Plant::None => {
                sync(machine)?;
                count(machine)?;
            }
            Plant::CommitBeforeSync => {
                count(machine)?;
                sync(machine)?;
            }
            Plant::FailedSyncIgnored => {
                match sync(machine) {
                    Ok(_) | Err(Stop::Failed(_)) => {}
                    Err(lost) => return Err(lost),
                }
                count(machine)?;
            }
        }
    }
    Ok(())
}

struct Ledger(Machine);

impl World for Ledger {
    fn run(&mut self) -> Ended {
        let machine = &mut self.0;
        match catch_unwind(AssertUnwindSafe(|| ledger(machine))) {
            Ok(Ok(())) => Ended::Well,
            Ok(Err(Stop::PowerLost)) => Ended::PowerLost,
            Ok(Err(Stop::Failed(error))) => Ended::GaveUp(error),
            Err(payload) => Ended::Panicked(match payload.downcast::<String>() {
                Ok(message) => *message,
                Err(payload) => match payload.downcast::<&'static str>() {
                    Ok(message) => (*message).to_owned(),
                    Err(_) => "a panic with no message".to_owned(),
                },
            }),
        }
    }

    fn crash(&mut self) {
        self.0.sim.crash();
    }

    fn with_sim<R>(&mut self, f: impl FnOnce(&mut Sim) -> R) -> R {
        f(&mut self.0.sim)
    }
}

fn boot(plant: Plant) -> impl FnMut(u64) -> Ledger {
    move |seed| Ledger(Machine { sim: Sim::new(seed, Faults::default()), next: 0, plant })
}

/// The run that broke, or a panic saying the plant was not caught.
fn caught(plant: Plant, seeds: Seeds) -> (Failed, Run) {
    match run(seeds, Given::NOTHING, NAME, boot(plant)) {
        Err(failed) => {
            let broke = failed.broke().expect("a seed broke").clone(); // clone: a test's copy
            (failed, broke)
        }
        Ok(passed) => panic!(
            "the default seeds caught Plant::{plant:?} at none of {} seeds ({passed:?}): they, \
             or the ledger's oracle, are too weak",
            passed.seeds
        ),
    }
}

/// The invariant: every record the ledger committed is in its log, whole, at every start,
/// after any failed effect and any power cut of any default seed. (c): the correct ledger
/// passes all 256, and the seeds did inject: effects failed, power was cut, the program was
/// started again.
#[test]
fn the_ledger_passes_every_default_seed() {
    let passed = run(Seeds::DEFAULT, Given::NOTHING, NAME, boot(Plant::None))
        .unwrap_or_else(|failed| panic!("{failed}"));
    assert_eq!(passed.seeds, 256);
    assert!(passed.counts.failures > 100, "{passed:?}");
    assert!(passed.counts.cuts > 100, "{passed:?}");
    assert!(passed.starts > 2 * u64::from(passed.seeds), "{passed:?}");
}

/// (a), the first plant: a commit before its data's sync is right until power is lost between
/// the two.
#[test]
fn the_oracle_catches_a_commit_before_its_sync() {
    let (failed, broke) = caught(Plant::CommitBeforeSync, Seeds::DEFAULT);
    eprintln!("{failed}");
    assert!(broke.verdict.as_ref().is_err_and(|it| it.why.contains("committed is lost or torn")), "{failed}");
    assert!(matches!(failed, Failed::Seed { replay: Replay::Same, .. }), "{failed}");
}

/// (a), the second plant: a failed sync ignored.
#[test]
fn the_oracle_catches_a_failed_sync_ignored() {
    let (failed, broke) = caught(Plant::FailedSyncIgnored, Seeds::DEFAULT);
    eprintln!("{failed}");
    assert!(broke.verdict.as_ref().is_err_and(|it| it.why.contains("committed is lost or torn")), "{failed}");
    assert!(broke.counts.failures > 0 && broke.counts.cuts > 0, "it takes a failed sync and a cut: {failed}");
    assert!(matches!(failed, Failed::Seed { replay: Replay::Same, .. }), "{failed}");
}

/// (b): the seed is read from the line the failure prints, given back as the variable holds
/// it, and that run is the same failure with the same trace, byte for byte.
#[test]
fn the_printed_seed_replays_to_the_same_failure() {
    for plant in [Plant::CommitBeforeSync, Plant::FailedSyncIgnored] {
        let (failed, broke) = caught(plant, Seeds::DEFAULT);
        let printed = failed.to_string();
        let line = printed.lines().last().expect("the replay line");
        let given = line
            .strip_prefix("replay: BLAZE_SIM_SEED=")
            .and_then(|rest| rest.split_once(' '))
            .unwrap_or_else(|| panic!("no replay line: {line}"));
        assert_eq!(given.1, "blaze test -p blaze-core sim::seeds::tests::ledger -- --exact");
        let given = Given::parse(Some(given.0), None).expect("a seed");
        assert_eq!(given.seed, Some(broke.plan.seed));
        let replayed = match run(Seeds::DEFAULT, given, NAME, boot(plant)) {
            Err(Failed::Seed { at: 1, of: 1, run, replay: Replay::Same, .. }) => *run,
            other => panic!("the given seed did not fail alone and the same: {other:?}"),
        };
        assert_eq!(replayed.trace.as_bytes(), broke.trace.as_bytes());
        assert_eq!(replayed, broke);
    }
}

/// (d): one seed twice is one run, its trace the same bytes, with or without a plant.
#[test]
fn one_seed_twice_is_one_trace() {
    for plant in [Plant::None, Plant::FailedSyncIgnored] {
        for seed in 0..64 {
            let first = one(seed, &Seeds::DEFAULT, &mut boot(plant));
            let second = one(seed, &Seeds::DEFAULT, &mut boot(plant));
            assert_eq!(first.trace.as_bytes(), second.trace.as_bytes(), "seed {seed}, {plant:?}");
            assert_eq!(first, second, "seed {seed}, {plant:?}");
            assert!(first.trace.lines().count() > 4, "seed {seed}: {}", first.trace);
        }
    }
    // And another seed is another run: the trace is the seed's, not the program's alone.
    let traces: Vec<String> = (0..64).map(|seed| one(seed, &Seeds::DEFAULT, &mut boot(Plant::None)).trace).collect();
    let distinct = traces.iter().collect::<std::collections::BTreeSet<_>>().len();
    assert!(distinct > 32, "{distinct} distinct traces of 64 seeds");
}

/// The cause of (a) is the crash: with crashes left out by name both plants pass every seed
/// and no power is cut, while failures are still injected.
#[test]
fn without_crashes_no_power_is_cut() {
    let seeds = Seeds::DEFAULT.without(Fault::Crashes);
    for plant in [Plant::None, Plant::CommitBeforeSync, Plant::FailedSyncIgnored] {
        let passed = run(seeds, Given::NOTHING, NAME, boot(plant)).unwrap_or_else(|failed| panic!("{failed}"));
        assert_eq!((passed.seeds, passed.counts.cuts), (256, 0), "{plant:?}");
        assert!(passed.counts.failures > 0, "{plant:?}: {passed:?}");
    }
    // And with failures left out too, nothing is injected: every seed is one start.
    let kind = seeds.without(Fault::IoFailures);
    let passed = run(kind, Given::NOTHING, NAME, boot(Plant::None)).unwrap_or_else(|failed| panic!("{failed}"));
    assert_eq!((passed.starts, passed.counts.failures, passed.counts.cuts), (256, 0, 0));
}

/// Leaving a kind out zeroes it and changes nothing else of a seed's plan, and every kind is
/// on in some default seeds and off in others (the swarm).
#[test]
fn leaving_a_fault_out_changes_no_other_draw() {
    let (mut fails, mut cuts, mut delays) = (0, 0, 0);
    for seed in 0..256 {
        let all = Plan::of(seed, &Seeds::DEFAULT);
        fails += u32::from(all.fail_one_in != 0);
        cuts += u32::from(all.crash_one_in != 0);
        delays += u32::from(!all.max_latency.is_zero());
        let without = |fault| Plan::of(seed, &Seeds::DEFAULT.without(fault));
        assert_eq!(without(Fault::IoFailures), Plan { fail_one_in: 0, ..all });
        assert_eq!(without(Fault::Crashes), Plan { crash_one_in: 0, ..all });
        assert_eq!(without(Fault::Delays), Plan { max_latency: Duration::ZERO, ..all });
    }
    for on in [fails, cuts, delays] {
        assert!((64..=224).contains(&on), "a kind is on in {on} of 256 seeds");
    }
    assert!(Fault::ALL.iter().all(|&fault| Seeds::DEFAULT.injects(fault)));
    assert!(!Seeds::DEFAULT.without(Fault::Delays).injects(Fault::Delays));
}

/// What is given from outside is typed: a seed in decimal or hex, a count of one or more, and
/// anything else refused naming its variable. A given count runs that many seeds.
#[test]
fn what_is_given_is_typed_or_refused() {
    assert_eq!(Given::parse(None, None), Ok(Given::NOTHING));
    assert_eq!(Given::parse(Some("41"), None).map(|it| it.seed), Ok(Some(41)));
    assert_eq!(Given::parse(Some("0x29"), Some("7")), Ok(Given { seed: Some(41), count: Some(7) }));
    let refused = Given::parse(Some("forty-one"), None).unwrap_err();
    assert!(refused.contains("BLAZE_SIM_SEED=\"forty-one\""), "{refused}");
    for count in ["0", "-3", "many"] {
        let refused = Given::parse(None, Some(count)).unwrap_err();
        assert!(refused.contains("BLAZE_SIM_SEEDS="), "{refused}");
    }
    let given = Given { seed: None, count: Some(9) };
    let passed = run(Seeds::DEFAULT, given, NAME, boot(Plant::None)).unwrap_or_else(|failed| panic!("{failed}"));
    assert_eq!(passed.seeds, 9);
}

/// A world whose program does what `run` says and asks the simulation for nothing.
struct Fixed(Sim, Ended);

impl World for Fixed {
    fn run(&mut self) -> Ended {
        self.1.clone() // clone: a test's fixed end
    }

    fn crash(&mut self) {
        self.0.crash();
    }

    fn with_sim<R>(&mut self, f: impl FnOnce(&mut Sim) -> R) -> R {
        f(&mut self.0)
    }
}

/// A test that asks the simulation for nothing fails, naming itself; a program that still
/// gives up once every fault is healed fails with its error; and a panic fails at its start.
#[test]
fn a_run_that_tests_nothing_and_one_that_never_ends_well_fail_by_name() {
    let fixed = |ended: Ended| move |seed| Fixed(Sim::new(seed, Faults::default()), ended.clone()); // clone: a test's fixed end
    let idle = run(Seeds::DEFAULT, Given::NOTHING, NAME, fixed(Ended::Well)).unwrap_err();
    assert_eq!(
        idle,
        Failed::Idle { package: "blaze-core".to_owned(), test: NAME.test.to_owned(), seeds: 256 }
    );
    assert!(idle.to_string().contains("asked the simulation for nothing in any of its 256 seeds"), "{idle}");

    let gave_up = run(Seeds::DEFAULT, Given::NOTHING, NAME, fixed(Ended::GaveUp("no room".to_owned()))).unwrap_err();
    let broke = gave_up.broke().expect("a seed broke");
    let why = &broke.verdict.as_ref().unwrap_err().why;
    assert_eq!(why, "with no fault left to blame, the program gave up: no room");
    // Bounded: at most `restarts_most` starts under faults, and the one after they heal.
    assert!(broke.starts <= Seeds::DEFAULT.restarts_most + 1, "{gave_up}");

    let panicked = run(Seeds::DEFAULT, Given::NOTHING, NAME, fixed(Ended::Panicked("boom".to_owned()))).unwrap_err();
    let broke = panicked.broke().expect("a seed broke");
    assert_eq!(broke.verdict, Err(Broke { start: 1, why: "panicked: boom".to_owned() }));
}

/// A machine whose power was cut takes no effect and tells no completion until it is turned
/// on: what still runs of a program after the cut reaches nothing.
#[test]
fn a_machine_that_is_off_takes_nothing() {
    let plan = Plan { seed: 7, fail_one_in: 0, max_latency: Duration::ZERO, crash_one_in: 1, cuts_most: 1 };
    let mut sim = Sim::new(7, Faults::default());
    sim.arm(&plan);
    let file = name("a/file");
    // clone: the effects' names, refcounts.
    let append = |tag| Effect::Append { file: file.clone(), bytes: vec![1; 4], tag: Tag(tag) };
    sim.submit(&mut vec![append(0)]);
    assert!(sim.cut_due(), "one in one: the first effect cuts the power");
    assert!(!sim.cut_due(), "a cut is due once");
    sim.crash();
    assert!(sim.is_off());
    sim.submit(&mut vec![append(1), Effect::Sync { file: file.clone(), tag: Tag(2) }]);
    let mut told = Vec::new();
    assert_eq!(sim.poll(&mut told, Deadline::NEVER), 0);
    assert_eq!(sim.in_flight(), 0);
    assert_eq!(sim.counts(), Counts { effects: 1, dropped: 2, failures: 0, cuts: 1 });
    sim.power_on();
    assert!(!sim.is_off());
    sim.submit(&mut vec![append(3)]);
    assert!(!sim.cut_due(), "the plan's one cut is spent");
    assert_eq!(sim.poll(&mut told, Deadline::NEVER), 1);
    assert_eq!(sim.contents(&file), [1; 4], "only the append after the power came back landed");
    let trace = sim.trace().to_owned();
    assert!(trace.contains("power lost at an effect: the machine is off, 1 effects in flight"), "{trace}");
    assert_eq!(trace.matches("off, dropped").count(), 2, "{trace}");
}

/// A trace line keeps [`LINE_MOST`] bytes of what it says and stands for the rest by its
/// length and a hash of it, so two long writes that differ give two lines.
#[test]
fn a_long_line_is_cut_and_still_tells_what_it_held() {
    let line = |byte: u8| {
        let mut trace = Trace::default();
        let effect = Effect::Append { file: name("big"), bytes: vec![byte; 4096], tag: Tag(1) };
        trace.line(Duration::from_nanos(5), format_args!("> {effect:?}"));
        trace.text
    };
    let (ones, twos, last) = (line(1), line(2), {
        let mut trace = Trace::default();
        let mut bytes = vec![1; 4096];
        bytes[4095] = 9;
        let effect = Effect::Append { file: name("big"), bytes, tag: Tag(1) };
        trace.line(Duration::from_nanos(5), format_args!("> {effect:?}"));
        trace.text
    });
    assert!(ones.len() < LINE_MOST + 80 && ones.ends_with('\n'), "{ones}");
    assert!(ones.contains(" more bytes, hash "), "{ones}");
    assert_ne!(ones, twos);
    assert_ne!(ones, last, "a difference past the cut is in the hash");
    assert_eq!(ones, line(1));
    // A short line is itself.
    let mut trace = Trace::default();
    trace.line(Duration::from_nanos(5), format_args!("start 1"));
    assert_eq!(trace.text, "             5 start 1\n");
}
