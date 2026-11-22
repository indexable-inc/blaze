//! `#[bstd::test]` with grants (D109, crates/blaze-std/src/simtest/granted.rs): a test names
//! what it takes after `cx`, each parameter made by its type from the test's simulated world.
//!
//! - A folder is granted with the files the attribute gave it, and nothing beside it is
//!   reachable through it (control: the sibling folder's file is there, through its own grant).
//! - An environment and a command line are what the attribute set, and a variable it did not
//!   name is withheld (control: the one it named is read).
//! - The fault kinds a seed explores are those the parameters can meet ([`reach`]).
//! - A test of `cx` alone runs ONCE; the control, the same body with a `Clock`, runs every seed.
//! - The failure report is one block: name, what it took, the seed and faults in play, why,
//!   what `out` wrote, the trace's tail, one replay line (and none of the seed for a test run
//!   once).

use core::{cell::Cell, time::Duration};

use super::path;
use crate::{
    Cx,
    args::Args,
    clock::Clock,
    env::{Env, VarName},
    fs::{Dir, FsError, FsKind, ReadOnly},
    grant::{Given, Kind, LaunchAlloc, Manifest, Param, Refused},
    out::Out,
    test::{ALL_FAULTS, Fault, Given as SeedsGiven, Planned, Setup, Test, bit, report, simulated},
};

/// The test entry's heap, as a test hands it (D102).
fn alloc() -> LaunchAlloc {
    blaze_own::testing::heap().shared()
}

/// A folder granted read only holds the fixture's files the attribute gave it; the sibling
/// folder's file is not reachable through it, and is through its own grant (the control: the
/// file is there, so its absence from `data` is the narrowing, not a missing file).
#[crate::test(
    data = files("ledger/head" => b"7\n", "ledger/log" => b"a\nb\n"),
    other = files("secret" => b"kept\n"),
)]
fn a_folder_is_granted_with_its_fixture_and_nothing_beside_it(
    cx: &Cx,
    data: Dir<ReadOnly>,
    other: Dir<ReadOnly>,
) -> Result<(), FsError> {
    let mut head = cx.bytes();
    data.read_all(cx, &path("ledger/head"), &mut head)?;
    assert_eq!(head.as_slice(), b"7\n");
    let mut log = cx.bytes();
    data.read_all(cx, &path("ledger/log"), &mut log)?;
    assert_eq!(log.as_slice(), b"a\nb\n");
    let mut beside = cx.bytes();
    match data.read_all(cx, &path("other/secret"), &mut beside) {
        Err(error) if matches!(error.kind, FsKind::NotFound) => {}
        Err(error) => return Err(error),
        Ok(_) => panic!("`data` reached its sibling folder's file"),
    }
    let mut secret = cx.bytes();
    other.read_all(cx, &path("secret"), &mut secret)?;
    assert_eq!(secret.as_slice(), b"kept\n");
    Ok(())
}

/// The environment and the command line are the attribute's; a variable it did not set is
/// withheld by the `Env` grant (which reads only the names the attribute gave).
/// Expected: `test tests::test_grants::the_environment_and_the_command_line_are_the_attribute_s ... ok`.
#[crate::test(env(LANG = "C"), args("--fast", "in.txt"))]
fn the_environment_and_the_command_line_are_the_attribute_s(_cx: &Cx, env: Env, args: Args) {
    let lang = VarName::new("LANG").expect("a name");
    assert_eq!(env.get(&lang), Ok(Some("C")));
    let home = VarName::new("HOME").expect("a name");
    assert!(env.get(&home).is_err(), "a variable the attribute did not set is withheld");
    assert_eq!(args.get(0), Some("--fast"));
    assert_eq!(args.get(1), Some("in.txt"));
    assert_eq!(args.get(2), None);
}

/// The fault kinds by parameter: a folder that writes meets every kind; one read only meets
/// failures and late completions, no power cut; a clock only late completions; `cx` alone,
/// `Out`, `Args` and `Env` none, so the test is run once. Each line would fail if `reach`
/// gave every kind to every parameter (the old attribute's behaviour).
#[test]
fn the_faults_a_seed_explores_are_those_its_parameters_meet() {
    const WRITES: [Param; 1] = [Param::new("data", Kind::Dir { write: true })];
    const READS: [Param; 1] = [Param::new("data", Kind::Dir { write: false })];
    const CLOCK: [Param; 1] = [Param::new("clock", Kind::Clock)];
    const QUIET: [Param; 3] = [Param::new("out", Kind::Out), Param::new("args", Kind::Args), Param::new("env", Kind::Env)];
    let writes = Planned::of(&Setup::DEFAULT, &WRITES);
    assert_eq!((writes.faults, writes.once), (ALL_FAULTS, false));
    let reads = Planned::of(&Setup::DEFAULT, &READS);
    assert_eq!(reads.faults, bit(Fault::IoFailures) | bit(Fault::Delays));
    assert!(!reads.seeds.injects(Fault::Crashes), "a read-only folder is torn by no crash");
    let clock = Planned::of(&Setup::DEFAULT, &CLOCK);
    assert_eq!(clock.faults, bit(Fault::Delays));
    let quiet = Planned::of(&Setup::DEFAULT, &QUIET);
    assert_eq!((quiet.faults, quiet.once), (0, true));
    let alone = Planned::of(&Setup::DEFAULT, &[]);
    assert_eq!((alone.faults, alone.once), (0, true));
    // `without` narrows what the parameters reach, and only that.
    let narrowed = Planned::of(&Setup::DEFAULT.without(Fault::Crashes), &WRITES);
    assert_eq!(narrowed.faults, bit(Fault::IoFailures) | bit(Fault::Delays));
}

static NO_PARAMS: Manifest = Manifest::new(&[]);
static A_CLOCK: Manifest = Manifest::new(&[Param::new("clock", Kind::Clock)]);
static OUT_AND_DATA: Manifest =
    Manifest::new(&[Param::new("out", Kind::Out), Param::new("data", Kind::Dir { write: true })]);
static DEFAULT: Setup = Setup::DEFAULT;

/// The test `name` of `manifest`, set up by default, planned by its parameters.
fn test_of(name: &'static str, manifest: &'static Manifest) -> Test {
    Test {
        package: "blaze-std",
        module: module_path!(),
        name,
        manifest,
        setup: &DEFAULT,
        planned: Planned::of(&DEFAULT, manifest.params),
    }
}

/// A test of `cx` alone is a pure function of its code: it is run once. The control is the
/// same counting body taking a `Clock` (and sleeping on it), which every default seed runs:
/// so the single run is the plan's doing, not the body's.
/// Expected: `test tests::test_grants::a_test_of_cx_alone_runs_once ... ok`.
#[test]
fn a_test_of_cx_alone_runs_once() {
    let runs = Cell::new(0u32);
    let alone = |given: &mut Given<'_>| -> Result<(), Refused> {
        let _filled = given.checking().filled()?;
        runs.set(runs.get() + 1);
        Ok(())
    };
    let passed = simulated(&test_of("alone", &NO_PARAMS), SeedsGiven::NOTHING, alloc(), &alone)
        .unwrap_or_else(|broken| panic!("{}", broken.failed));
    assert_eq!((runs.get(), passed.seeds), (1, 1));

    runs.set(0);
    let clocked = |given: &mut Given<'_>| -> Result<Result<(), crate::clock::SleepError>, Refused> {
        let filled = given.checking().check::<Clock>("clock").filled()?;
        runs.set(runs.get() + 1);
        let cx = filled.cx();
        let ((), clock) = filled.hand_out();
        Ok(clock.sleep(cx, Duration::from_millis(1)))
    };
    let passed = simulated(&test_of("clocked", &A_CLOCK), SeedsGiven::NOTHING, alloc(), &clocked)
        .unwrap_or_else(|broken| panic!("{}", broken.failed));
    assert_eq!(passed.seeds, 256);
    assert!(runs.get() >= 256, "every seed started the body at least once: {}", runs.get());
}

/// The failure report is one block: the test's name and package, what it took, the seed and
/// the faults in play by name, why (the failing check with its values), what `out` wrote, the
/// trace's tail, and ONE replay line. Before D109 the block said none of what the test took,
/// named no fault, and showed nothing the program printed.
/// Expected: `test tests::test_grants::the_failure_report_is_one_block_with_one_replay_line ... ok`.
#[test]
fn the_failure_report_is_one_block_with_one_replay_line() {
    let body = |given: &mut Given<'_>| -> Result<(), Refused> {
        let filled = given.checking().check::<Out>("out").check::<Dir<crate::fs::ReadWrite>>("data").filled()?;
        let cx = filled.cx();
        let (((), out), _data) = filled.hand_out();
        out.line(cx, "hello from the test").expect("standard output");
        let (left, right) = (1, 2);
        assert_eq!(left, right, "the count after a restart");
        Ok(())
    };
    let test = test_of("report", &OUT_AND_DATA);
    let broken = match simulated(&test, SeedsGiven::NOTHING, alloc(), &body) {
        Ok(passed) => panic!("a body that always fails passed: {passed:?}"),
        Err(broken) => broken,
    };
    let crate::test::Failed::Seed { run, at, of, replay, .. } = &broken.failed else {
        panic!("not a seed's failure: {}", broken.failed)
    };
    // The seed broke by the body's check, not by its folder's making: a fault met while the
    // grants are made (each made once) is the start failing, restarted.
    let why = &run.verdict.as_ref().expect_err("a broken seed").why;
    assert!(why.contains("the count after a restart"), "{why}");
    let mut text = blaze_own::String::new_in(alloc());
    report(&test, "tests::test_grants::report", run, (*at, *of), replay, broken.last.as_ref(), &mut text);
    let text = text.as_str();
    eprintln!("{text}");
    assert!(text.starts_with("`tests::test_grants::report` of blaze-std FAILED\n"), "{text}");
    assert!(text.contains("\ntakes: cx, out: out, data: dir (writes)\n"), "{text}");
    assert!(text.contains("\nfaults in play: IoFailures, Delays, Crashes ("), "{text}");
    assert!(text.contains("the count after a restart"), "{text}");
    assert!(text.contains("left: 1") && text.contains("right: 2"), "the failing check's values: {text}");
    assert!(text.contains("\nout (") && text.contains("\n  hello from the test\n"), "{text}");
    let last = text.lines().last().expect("a last line");
    let seed = run.plan.seed;
    assert_eq!(
        last,
        alloc::format!("replay: BLAZE_SIM_SEED={seed} blaze test -p blaze-std tests::test_grants::report -- --exact")
    );
    assert_eq!(text.matches("replay: ").count(), 1, "one replay line: {text}");
}

/// A test run once replays with no seed: its replay line names none.
#[test]
fn a_test_run_once_replays_with_no_seed() {
    let body = |given: &mut Given<'_>| -> Result<(), Refused> {
        let _filled = given.checking().filled()?;
        panic!("a pure check failed");
    };
    let test = test_of("pure", &NO_PARAMS);
    let broken = match simulated(&test, SeedsGiven::NOTHING, alloc(), &body) {
        Ok(passed) => panic!("a body that always fails passed: {passed:?}"),
        Err(broken) => broken,
    };
    let crate::test::Failed::Seed { run, at, of, replay, .. } = &broken.failed else {
        panic!("not a seed's failure: {}", broken.failed)
    };
    let mut text = blaze_own::String::new_in(alloc());
    report(&test, "tests::test_grants::pure", run, (*at, *of), replay, broken.last.as_ref(), &mut text);
    let text = text.as_str();
    assert!(text.contains("\ntakes: cx alone\n") && text.contains("\nrun once: "), "{text}");
    assert_eq!(text.lines().last(), Some("replay: blaze test -p blaze-std tests::test_grants::pure -- --exact"));
}

/// A value whose drop counts itself in the cell it borrows.
struct Counted<'c>(&'c Cell<u32>);

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

/// A test takes the heap as it takes any grant (D120): a blaze-own `Vec` grows in
/// `heap.alloc()`, and a future pinned in `heap` itself runs to its value; both are dropped,
/// each value once. The heap meets no fault, so the test runs once.
/// Prints `test tests::test_grants::a_test_is_handed_the_heap_and_pins_a_future_in_it ... ok`.
#[crate::test]
fn a_test_is_handed_the_heap_and_pins_a_future_in_it(_cx: &Cx, heap: crate::mem::Heap) {
    let dropped = Cell::new(0);
    let mut values = crate::vec::Vec::new_in(heap.alloc());
    values.push(Counted(&dropped));
    values.push(Counted(&dropped));
    assert_eq!(values.len(), 2);
    drop(values);
    assert_eq!(dropped.get(), 2, "the vector dropped each value once");

    let counted = Counted(&dropped);
    let mut pinned: core::pin::Pin<crate::boxed::Box<dyn Future<Output = u32> + '_, crate::mem::Heap>> =
        crate::boxed::Box::pin_in(
            async move {
                let _counted = counted;
                5u32
            },
            heap,
        );
    let mut context = core::task::Context::from_waker(core::task::Waker::noop());
    assert_eq!(pinned.as_mut().poll(&mut context), core::task::Poll::Ready(5));
    assert_eq!(dropped.get(), 3, "the future dropped what it held as it finished");
    drop(pinned);
    assert_eq!(dropped.get(), 3, "nothing is dropped twice");
}
