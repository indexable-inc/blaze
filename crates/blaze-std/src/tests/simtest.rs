//! `#[bstd::test]` on bstd's own files (docs/v2/plan/design/bstd-sim-test.md, "Proof"; D109):
//! a counter kept in two files beneath the folder it is granted, `value` published and then
//! `commit`, each holding the count. Its start is its recovery and its oracle: the value is the
//! committed count or one past it, and each file is whole.
//!
//! The counter asks nothing of the harness: it makes no world, loops no seed, turns no fault
//! on and never crashes its machine. It names the one folder it writes (`counter:
//! Dir<ReadWrite>`), and the attribute does the rest: a folder that writes meets every fault
//! kind, so every kind is on by default.
//!
//! - (c) the correct counter passes every default seed, through the attribute itself.
//! - (a) each planted bug ([`Bug`]) is caught within the default seeds: a value published
//!   unsynced (right until power is lost), and a failed publish ignored.
//! - (b) the seed a failure's report prints, given back, is the same failure and the same
//!   trace, byte for byte.
//! - (d) one seed twice is one trace.
//! - The opt-out is a named argument and it is the cause: with crashes left out, the value
//!   published unsynced passes every seed (test-the-cause).
//!
//! The runner's own proof, which cargo runs, is crates/blaze-core/src/sim/seeds/tests.rs.

use super::path;
use crate::{
    Cx,
    fs::{Content, Dir, FsError, FsKind, ReadWrite, Unsynced},
    grant::{Given, Kind, LaunchAlloc, Manifest, Param, Refused},
    test::{Failed, Fault, Given as SeedsGiven, Planned, Replay, Setup, Test, one_seed, report, simulated},
};

/// A safeguard of the counter's removed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bug {
    None,
    /// The value is published with nothing synced: right until power is lost.
    ValueUnsynced,
    /// A publish of the value that failed is taken as done, and its count committed.
    FailedPublishIgnored,
}

/// The count `at` holds, `None` for a file that is not there. A file that holds no whole
/// count is the oracle's finding.
fn count(cx: &Cx, dir: &Dir<ReadWrite>, at: &str) -> Result<Option<u64>, FsError> {
    let mut into = cx.bytes();
    match dir.read_all(cx, &path(at), &mut into) {
        Ok(_) => {
            let held: Result<[u8; 8], _> = into.as_slice().try_into();
            match held {
                Ok(bytes) => Ok(Some(u64::from_le_bytes(bytes))),
                Err(_) => panic!("{at} holds {} bytes: a count torn", into.len()),
            }
        }
        Err(error) if matches!(error.kind, FsKind::NotFound) => Ok(None),
        Err(error) => Err(error),
    }
}

/// One start of the counter in the folder it was granted: recovery, then three more counts.
fn run_counter(cx: &Cx, dir: &Dir<ReadWrite>, bug: Bug) -> Result<(), FsError> {
    let committed = count(cx, dir, "commit")?.unwrap_or(0);
    let value = count(cx, dir, "value")?.unwrap_or(0);
    assert!(
        value == committed || value == committed + 1,
        "the commit says {committed} and the value is {value}"
    );
    for next in committed + 1..=committed + 3 {
        let bytes = next.to_le_bytes();
        match bug {
            Bug::None => _ = dir.publish_synced(cx, &path("value"), Content::bytes(&bytes))?,
            Bug::ValueUnsynced => {
                _ = dir.publish_unsynced(cx, &path("value"), Content::bytes(&bytes), Unsynced::No)?;
            }
            Bug::FailedPublishIgnored => {
                _ = dir.publish_synced(cx, &path("value"), Content::bytes(&bytes));
            }
        }
        _ = dir.publish_synced(cx, &path("commit"), Content::bytes(&bytes))?;
    }
    Ok(())
}

/// The counter test's manifest: the one folder it writes.
static COUNTER: Manifest = Manifest::new(&[Param::new("counter", Kind::Dir { write: true })]);
static DEFAULT: Setup = Setup::DEFAULT;
static NO_CRASHES: Setup = Setup::DEFAULT.without(Fault::Crashes);

/// The counter as a test of the runner, set up by `setup`.
fn counter_test(setup: &'static Setup) -> Test {
    Test {
        package: "blaze-std",
        module: module_path!(),
        name: "counter",
        manifest: &COUNTER,
        setup,
        planned: Planned::of(setup, COUNTER.params),
    }
}

/// The launcher's body of the counter with `bug` planted: the folder checked, made, counted in.
fn buggy(bug: Bug) -> impl Fn(&mut Given<'_>) -> Result<Result<(), FsError>, Refused> {
    move |given: &mut Given<'_>| {
        let filled = given.checking().check::<Dir<ReadWrite>>("counter").filled()?;
        let cx = filled.cx();
        let ((), dir) = filled.hand_out();
        Ok(run_counter(cx, &dir, bug))
    }
}

/// The test entry's heap, as a test hands it (D102).
fn alloc() -> LaunchAlloc {
    blaze_own::testing::heap().shared()
}

/// The counter under the default seeds, as the attribute runs it, with nothing given.
fn failure(bug: Bug) -> crate::test::Broken {
    match simulated(&counter_test(&DEFAULT), SeedsGiven::NOTHING, alloc(), &buggy(bug)) {
        Err(broken) => broken,
        Ok(passed) => panic!(
            "the default seeds caught Bug::{bug:?} at none of {} seeds ({passed:?}): they, or \
             the counter's oracle, are too weak",
            passed.seeds
        ),
    }
}

/// The invariant: at every start the counter's value is its committed count or one past it,
/// and both files are whole, after any failed effect and any power cut of any default seed.
/// (c), through the attribute: the test is the program, names the folder it writes, and names
/// no seed and no fault.
#[crate::test]
fn the_counter_keeps_what_it_committed(cx: &Cx, counter: Dir<ReadWrite>) -> Result<(), FsError> {
    run_counter(cx, &counter, Bug::None)
}

/// The same, counted: all 256 seeds pass, and they did inject failures and cut the power.
#[test]
fn the_default_seeds_inject_and_the_counter_passes() {
    let passed = simulated(&counter_test(&DEFAULT), SeedsGiven::NOTHING, alloc(), &buggy(Bug::None))
        .unwrap_or_else(|broken| panic!("{}", broken.failed));
    assert_eq!(passed.seeds, 256);
    assert!(passed.counts.failures > 0 && passed.counts.cuts > 0, "{passed:?}");
    assert!(passed.starts > u64::from(passed.seeds), "{passed:?}");
}

/// (a), the first plant: a value published unsynced is right until power is lost.
#[test]
fn the_oracle_catches_a_value_published_unsynced() {
    let failed = failure(Bug::ValueUnsynced).failed;
    eprintln!("{failed}");
    let broke = failed.broke().expect("a seed broke");
    assert!(broke.counts.cuts > 0, "only a power cut shows it: {failed}");
    assert!(matches!(failed, Failed::Seed { replay: Replay::Same, .. }), "{failed}");
}

/// (a), the second plant: a failed publish ignored.
#[test]
fn the_oracle_catches_a_failed_publish_ignored() {
    let failed = failure(Bug::FailedPublishIgnored).failed;
    eprintln!("{failed}");
    let broke = failed.broke().expect("a seed broke");
    assert!(broke.counts.failures > 0, "only a failed effect shows it: {failed}");
    assert!(matches!(failed, Failed::Seed { replay: Replay::Same, .. }), "{failed}");
}

/// (b): the seed is read from the line the failure's REPORT prints (the one block a person
/// sees), given back as the variable holds it, and that run is the same failure with the same
/// trace, byte for byte.
#[test]
fn the_printed_seed_replays_to_the_same_failure() {
    let test = counter_test(&DEFAULT);
    for bug in [Bug::ValueUnsynced, Bug::FailedPublishIgnored] {
        let broken = failure(bug);
        let Failed::Seed { run, at, of, replay, .. } = &broken.failed else {
            panic!("not a seed's failure: {}", broken.failed)
        };
        let mut printed = blaze_own::String::new_in(alloc());
        report(&test, "tests::simtest::counter", run, (*at, *of), replay, broken.last.as_ref(), &mut printed);
        let line = printed.as_str().lines().last().expect("the replay line");
        let (seed, command) = line
            .strip_prefix("replay: BLAZE_SIM_SEED=")
            .and_then(|rest| rest.split_once(' '))
            .unwrap_or_else(|| panic!("no replay line: {line}"));
        assert_eq!(command, "blaze test -p blaze-std tests::simtest::counter -- --exact");
        let given = SeedsGiven::parse(Some(seed), None).expect("a seed");
        assert_eq!(given.seed, Some(run.plan.seed));
        let replayed = simulated(&test, given, alloc(), &buggy(bug)).map_err(|broken| broken.failed);
        let replayed = match &replayed {
            Err(replayed @ Failed::Seed { at: 1, of: 1, replay: Replay::Same, .. }) => replayed.broke().expect("a seed"),
            other => panic!("the given seed did not fail alone and the same: {other:?}"),
        };
        assert_eq!(replayed.trace.as_bytes(), run.trace.as_bytes(), "{bug:?}");
        assert_eq!(replayed, &**run, "{bug:?}");
    }
}

/// (d): one seed twice is one run, its trace the same bytes, with or without a plant.
#[test]
fn one_seed_twice_is_one_trace() {
    let test = counter_test(&DEFAULT);
    for bug in [Bug::None, Bug::ValueUnsynced] {
        for seed in 0..32 {
            let first = one_seed(&test, seed, alloc(), &buggy(bug)).expect("the counter's setup");
            let second = one_seed(&test, seed, alloc(), &buggy(bug)).expect("the counter's setup");
            assert_eq!(first.trace.as_bytes(), second.trace.as_bytes(), "seed {seed}, {bug:?}");
            assert_eq!(first, second, "seed {seed}, {bug:?}");
            assert!(first.counts.effects > 0, "seed {seed}: the counter asked the simulation for nothing");
        }
    }
}

/// The opt-out is a named argument of the attribute, and the crash is the cause: the value
/// published unsynced, which the default seeds catch, passes every seed with crashes left out.
#[crate::test(without(Crashes))]
fn without_crashes_an_unsynced_value_passes(cx: &Cx, counter: Dir<ReadWrite>) -> Result<(), FsError> {
    run_counter(cx, &counter, Bug::ValueUnsynced)
}

/// The same, counted: no power is cut, and failures are still injected.
#[test]
fn without_crashes_no_power_is_cut() {
    let passed = simulated(&counter_test(&NO_CRASHES), SeedsGiven::NOTHING, alloc(), &buggy(Bug::ValueUnsynced))
        .unwrap_or_else(|broken| panic!("{}", broken.failed));
    assert_eq!((passed.seeds, passed.counts.cuts), (256, 0));
    assert!(passed.counts.failures > 0, "{passed:?}");
}

/// A clock the test takes and never waits on: its making asks the simulation nothing (a
/// folder's would: its open is an effect), and a clock meets late completions, so it is run
/// under seeds.
static A_CLOCK: Manifest = Manifest::new(&[Param::new("clock", Kind::Clock)]);

/// A test that takes a clock and never waits on it asked the simulation for nothing, and is
/// told so by name. The control is the counter, which asks it for effects and passes
/// (`the_default_seeds_inject_and_the_counter_passes`).
#[test]
fn a_test_that_asks_the_simulation_for_nothing_fails_by_name() {
    let test = Test {
        package: "blaze-std",
        module: module_path!(),
        name: "idle",
        manifest: &A_CLOCK,
        setup: &DEFAULT,
        planned: Planned::of(&DEFAULT, A_CLOCK.params),
    };
    assert!(!test.planned.once, "a clock meets late completions: the test is run under seeds");
    let idle = |given: &mut Given<'_>| -> Result<(), Refused> {
        let _filled = given.checking().check::<crate::clock::Clock>("clock").filled()?;
        Ok(())
    };
    let idle = simulated(&test, SeedsGiven::NOTHING, alloc(), &idle).map_err(|broken| broken.failed);
    assert!(matches!(idle, Err(Failed::Idle { seeds: 256, .. })), "{idle:?}");
}
