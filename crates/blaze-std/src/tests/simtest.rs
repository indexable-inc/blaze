//! `#[bstd::test]` on bstd's own files (docs/v2/plan/design/bstd-sim-test.md, "Proof"): a
//! counter kept in two files beneath one directory, `value` published and then `commit`, each
//! holding the count. Its start is its recovery and its oracle: the value is the committed
//! count or one past it, and each file is whole.
//!
//! The counter asks nothing of the harness: it makes no world, loops no seed, turns no fault
//! on and never crashes its machine. The attribute does, by default.
//!
//! - (c) the correct counter passes every default seed, through the attribute itself.
//! - (a) each planted bug ([`Bug`]) is caught within the default seeds: a value published
//!   unsynced (right until power is lost), and a failed publish ignored.
//! - (b) the seed a failure prints, given back, is the same failure and the same trace, byte
//!   for byte.
//! - (d) one seed twice is one trace.
//! - The opt-out is a named argument and it is the cause: with crashes left out, the value
//!   published unsynced passes every seed (test-the-cause).
//!
//! The runner's own proof, which cargo runs, is crates/blaze-core/src/sim/seeds/tests.rs.

use super::path;
use crate::{
    Cx, Io,
    fs::{Content, Dir, FsError, FsKind, ReadWrite, Unsynced},
    test::{Failed, Fault, Given, Replay, Seeds, one, seeded},
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

/// One start of the counter: recovery, then three more counts.
fn counter(cx: &Cx, io: &Io, bug: Bug) -> Result<(), FsError> {
    let fs = io.need_fs().expect("the simulation offers files");
    let dir = fs.open_dir(cx, &path("counter"))?;
    let committed = count(cx, &dir, "commit")?.unwrap_or(0);
    let value = count(cx, &dir, "value")?.unwrap_or(0);
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

fn buggy(bug: Bug) -> impl Fn(&Cx, Io) -> Result<(), FsError> {
    move |cx, io| counter(cx, &io, bug)
}

/// The counter under the default seeds, as the attribute runs it, with nothing given.
fn failure(bug: Bug) -> Failed {
    match seeded("blaze-std", module_path!(), "counter", Seeds::DEFAULT, Given::NOTHING, buggy(bug)) {
        Err(failed) => failed,
        Ok(passed) => panic!(
            "the default seeds caught Bug::{bug:?} at none of {} seeds ({passed:?}): they, or \
             the counter's oracle, are too weak",
            passed.seeds
        ),
    }
}

/// The invariant: at every start the counter's value is its committed count or one past it,
/// and both files are whole, after any failed effect and any power cut of any default seed.
/// (c), through the attribute: the test is the program, and names no seed and no fault.
#[crate::test]
fn the_counter_keeps_what_it_committed(cx: &Cx, io: Io) -> Result<(), FsError> {
    counter(cx, &io, Bug::None)
}

/// The same, counted: all 256 seeds pass, and they did inject failures and cut the power.
#[test]
fn the_default_seeds_inject_and_the_counter_passes() {
    let passed = seeded("blaze-std", module_path!(), "counter", Seeds::DEFAULT, Given::NOTHING, buggy(Bug::None))
        .unwrap_or_else(|failed| panic!("{failed}"));
    assert_eq!(passed.seeds, 256);
    assert!(passed.counts.failures > 0 && passed.counts.cuts > 0, "{passed:?}");
    assert!(passed.starts > u64::from(passed.seeds), "{passed:?}");
}

/// (a), the first plant: a value published unsynced is right until power is lost.
#[test]
fn the_oracle_catches_a_value_published_unsynced() {
    let failed = failure(Bug::ValueUnsynced);
    eprintln!("{failed}");
    let broke = failed.broke().expect("a seed broke");
    assert!(broke.counts.cuts > 0, "only a power cut shows it: {failed}");
    assert!(matches!(failed, Failed::Seed { replay: Replay::Same, .. }), "{failed}");
}

/// (a), the second plant: a failed publish ignored.
#[test]
fn the_oracle_catches_a_failed_publish_ignored() {
    let failed = failure(Bug::FailedPublishIgnored);
    eprintln!("{failed}");
    let broke = failed.broke().expect("a seed broke");
    assert!(broke.counts.failures > 0, "only a failed effect shows it: {failed}");
    assert!(matches!(failed, Failed::Seed { replay: Replay::Same, .. }), "{failed}");
}

/// (b): the seed is read from the line the failure prints, given back as the variable holds
/// it, and that run is the same failure with the same trace, byte for byte.
#[test]
fn the_printed_seed_replays_to_the_same_failure() {
    for bug in [Bug::ValueUnsynced, Bug::FailedPublishIgnored] {
        let failed = failure(bug);
        let broke = failed.broke().expect("a seed broke");
        let printed = failed.to_string();
        let line = printed.lines().last().expect("the replay line");
        let (seed, command) = line
            .strip_prefix("replay: BLAZE_SIM_SEED=")
            .and_then(|rest| rest.split_once(' '))
            .unwrap_or_else(|| panic!("no replay line: {line}"));
        assert_eq!(command, "blaze test -p blaze-std tests::simtest::counter -- --exact");
        let given = Given::parse(Some(seed), None).expect("a seed");
        assert_eq!(given.seed, Some(broke.plan.seed));
        let replayed = seeded("blaze-std", module_path!(), "counter", Seeds::DEFAULT, given, buggy(bug));
        let replayed = match &replayed {
            Err(replayed @ Failed::Seed { at: 1, of: 1, replay: Replay::Same, .. }) => replayed.broke().expect("a seed"),
            other => panic!("the given seed did not fail alone and the same: {other:?}"),
        };
        assert_eq!(replayed.trace.as_bytes(), broke.trace.as_bytes(), "{bug:?}");
        assert_eq!(replayed, broke, "{bug:?}");
    }
}

/// (d): one seed twice is one run, its trace the same bytes, with or without a plant.
#[test]
fn one_seed_twice_is_one_trace() {
    for bug in [Bug::None, Bug::ValueUnsynced] {
        for seed in 0..32 {
            let first = one(seed, Seeds::DEFAULT, buggy(bug));
            let second = one(seed, Seeds::DEFAULT, buggy(bug));
            assert_eq!(first.trace.as_bytes(), second.trace.as_bytes(), "seed {seed}, {bug:?}");
            assert_eq!(first, second, "seed {seed}, {bug:?}");
            assert!(first.counts.effects > 0, "seed {seed}: the counter asked the simulation for nothing");
        }
    }
}

/// The opt-out is a named argument of the attribute, and the crash is the cause: the value
/// published unsynced, which the default seeds catch, passes every seed with crashes left out.
#[crate::test(without(Crashes))]
fn without_crashes_an_unsynced_value_passes(cx: &Cx, io: Io) -> Result<(), FsError> {
    counter(cx, &io, Bug::ValueUnsynced)
}

/// The same, counted: no power is cut, and failures are still injected.
#[test]
fn without_crashes_no_power_is_cut() {
    let seeds = Seeds::DEFAULT.without(Fault::Crashes);
    let passed = seeded("blaze-std", module_path!(), "counter", seeds, Given::NOTHING, buggy(Bug::ValueUnsynced))
        .unwrap_or_else(|failed| panic!("{failed}"));
    assert_eq!((passed.seeds, passed.counts.cuts), (256, 0));
    assert!(passed.counts.failures > 0, "{passed:?}");
}

/// A program that waits for nothing is no simulation test, and is told so by name.
#[test]
fn a_test_that_asks_the_simulation_for_nothing_fails_by_name() {
    let idle = seeded("blaze-std", module_path!(), "idle", Seeds::DEFAULT, Given::NOTHING, |_: &Cx, _: Io| {});
    assert!(matches!(idle, Err(Failed::Idle { seeds: 256, .. })), "{idle:?}");
}
