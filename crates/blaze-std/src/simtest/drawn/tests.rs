//! The drawn half of `#[bstd::test]` (D109), each claim with its control (rule 8):
//!
//! - a property that fails only for a LARGE input is reported with the SHRUNK input, and its
//!   replay line alone (seed and tape) fails it again with the same value; the same property
//!   with a bound it never meets passes (the control);
//! - equal seed and tape give byte-equal reports;
//! - a restart draws the values the first start drew; the planted bug (a restart drawing again
//!   from the live tape) is caught by the test's own oracle.
//!
//! Sources: blaze-sim's search and shrinker (crates/blaze-sim/src/search.rs, shrink.rs) and
//! `seeds::one`'s restarts (crates/blaze-core/src/sim/seeds.rs), as they are in this tree.

use core::cell::Cell;

use blaze_core::sim::seeds::{Ended, Fault, Given as SeedsGiven, Seeds, World};
use blaze_sim::{Tail, Tape};

use super::{Again, Case, DrawnGiven, DrawnWorld, run_with};
use crate::{
    cx::Cx,
    grant::{Given, LaunchAlloc, Manifest, Refused},
    mem::Frame,
    simtest::{
        granted::{Planned, Setup, Test, boot, spec_of},
        param::{Draws, Making, Param},
    },
};

static MANIFEST: Manifest = Manifest::new(&[]);
static SETUP: Setup = Setup::DEFAULT;

/// No fault: a test of `cx` and drawn values, run under one world seed.
const CALM: Seeds = Seeds::DEFAULT.without(Fault::IoFailures).without(Fault::Delays).without(Fault::Crashes);

fn test(name: &'static str) -> Test {
    Test {
        package: "blaze-std",
        module: core::module_path!(),
        name,
        manifest: &MANIFEST,
        setup: &SETUP,
        planned: Planned { faults: 0, once: false, seeds: CALM, drawn: 1 },
    }
}

fn alloc() -> LaunchAlloc {
    blaze_own::testing::heap().shared()
}

fn nothing_given() -> DrawnGiven<'static> {
    DrawnGiven { world: SeedsGiven::NOTHING, tape: None }
}

/// The body of `fn long_lists_fail(cx: &Cx, xs: Vec<u32, _>)` that fails when `xs` holds
/// `longest` elements or more, through the attribute's own body macro (so `_` is inferred as
/// the start's `&Frame`).
fn fails_from(longest: usize) -> impl Fn(&mut Given<'_>, &Draws<'_>) -> Result<(), Refused> {
    crate::__test_body!(_cx: &Cx, xs: alloc::vec::Vec<u32, _> ; {
        assert!(xs.len() < longest, "a list of {} elements", xs.len());
    })
}

/// The text between `start` and the next `end` in `report`.
fn between<'r>(report: &'r str, start: &str, end: char) -> &'r str {
    let Some(at) = report.find(start) else { panic!("the report has no `{start}`:\n{report}") };
    let rest = &report[at + start.len()..];
    match rest.find(end) {
        Some(stop) => &rest[..stop],
        None => rest,
    }
}

/// A list of 9 or more elements fails; the report shows it SHRUNK to nine zeros, and the
/// replay line's seed and tape alone fail it again with that value. The control, a bound of
/// 17 (above blaze-sim's longest drawn list, 16), passes. Prints
/// `drawn: shrunk to nine zeros, replayed from its line`.
#[test]
fn a_large_input_is_reported_shrunk_and_replays_from_its_line() {
    let test = test("a_large_input_is_reported_shrunk_and_replays_from_its_line");
    let report = match run_with(&test, nothing_given(), None, alloc(), &fails_from(9), Again::Replay) {
        Ok(seeds) => panic!("a list of 9 or more never drawn in {seeds} world seed(s)"),
        Err(report) => report,
    };
    let report = report.as_str();
    assert!(report.contains("  xs = [0, 0, 0, 0, 0, 0, 0, 0, 0]\n"), "not shrunk to nine zeros:\n{report}");
    let seed = between(report, "BLAZE_SIM_SEED=", ' ');
    let tape = between(report, "BLAZE_SIM_TAPE='", '\'');
    let world = SeedsGiven::parse(Some(seed), None).unwrap_or_else(|why| panic!("the replay line's seed: {why}"));
    let again = match run_with(&test, DrawnGiven { world, tape: Some(tape) }, None, alloc(), &fails_from(9), Again::Replay) {
        Ok(_) => panic!("the replay line's seed {seed} and tape `{tape}` passed:\n{report}"),
        Err(again) => again,
    };
    assert!(again.as_str().contains("  xs = [0, 0, 0, 0, 0, 0, 0, 0, 0]\n"), "the replay drew another value:\n{}", again.as_str());
    // The control: the same search, a bound no drawn list reaches.
    if let Err(report) = run_with(&test, nothing_given(), None, alloc(), &fails_from(17), Again::Replay) {
        panic!("the control failed:\n{}", report.as_str());
    }
    std::println!("drawn: shrunk to nine zeros, replayed from its line");
}

/// One test run twice gives byte-equal reports; so does its replay line run twice. Prints
/// `drawn: equal seed and tape, equal reports (<n> bytes)`.
#[test]
fn equal_seed_and_tape_give_byte_equal_reports() {
    let test = test("equal_seed_and_tape_give_byte_equal_reports");
    let once = || match run_with(&test, nothing_given(), None, alloc(), &fails_from(5), Again::Replay) {
        Ok(_) => panic!("a list of 5 or more never drawn"),
        Err(report) => report,
    };
    let (first, second) = (once(), once());
    assert_eq!(first.as_str(), second.as_str(), "two runs of one test gave two reports");
    let seed = between(first.as_str(), "BLAZE_SIM_SEED=", ' ');
    let tape = between(first.as_str(), "BLAZE_SIM_TAPE='", '\'');
    let replayed = || {
        let world = SeedsGiven::parse(Some(seed), None).unwrap_or_else(|why| panic!("{why}"));
        match run_with(&test, DrawnGiven { world, tape: Some(tape) }, None, alloc(), &fails_from(5), Again::Replay) {
            Ok(_) => panic!("the replay line passed"),
            Err(report) => report,
        }
    };
    assert_eq!(replayed().as_str(), replayed().as_str(), "two replays of one line gave two reports");
    std::println!("drawn: equal seed and tape, equal reports ({} bytes)", first.as_str().len());
}

/// Each start of one case gets the values the first start drew: two starts of a world over
/// 40 seeded tapes both end well, the body's oracle comparing each start's list with the
/// first's. Under the planted bug (a restart draws again from the live tape) the oracle
/// fails at a second start. Prints `drawn: restarts draw the first start's values; the planted
/// redraw is caught at <n> of 40 tapes`.
#[test]
fn a_restart_draws_what_the_first_start_drew() {
    let test = test("a_restart_draws_what_the_first_start_drew");
    let alloc = alloc();
    let spec = spec_of(&test, alloc).unwrap_or_else(|_| panic!("the test's spec"));
    let first: Cell<Option<(usize, u64)>> = Cell::new(None);
    let body = |given: &mut Given<'_>, draws: &Draws<'_>| -> Result<(), Refused> {
        let filled = given.checking().filled()?;
        let making = Making::new(filled.cx(), draws);
        let xs: alloc::vec::Vec<u32, &Frame> = Param::make(&making, (), "xs");
        let seen = (xs.len(), xs.iter().fold(0u64, |sum, &x| sum.wrapping_mul(31).wrapping_add(u64::from(x))));
        match first.get() {
            None => first.set(Some(seen)),
            Some(drawn) => assert_eq!(drawn, seen, "a restart drew another list"),
        }
        Ok(())
    };
    let starts = |again: Again, seed: u64| -> (Ended, Ended) {
        first.set(None);
        let tape = Tape::new(&[], Tail::Seeded(blaze_rng::SplitMix64::new(seed)), Tape::MOST_CHOICES);
        let case = Case {
            tape: &tape,
            replay: Tape::new(&[], Tail::Calm, Tape::MOST_CHOICES),
            record: core::cell::RefCell::new(blaze_sim::Record::new()),
            values: core::cell::RefCell::new(alloc::vec::Vec::new()),
            starts: Cell::new(0),
            shown: None,
            again,
        };
        let mut world = DrawnWorld { io: boot(0, &SETUP, &[]), spec: &spec, manifest: &MANIFEST, body: &body, alloc, case: &case };
        (world.run(), world.run())
    };
    for seed in 0..40 {
        assert_eq!(starts(Again::Replay, seed), (Ended::Well, Ended::Well), "tape {seed}: a restart did not get the first start's values");
    }
    let caught = (0..40).filter(|&seed| matches!(starts(Again::Live, seed).1, Ended::Panicked(_))).count();
    assert!(caught > 0, "the planted redraw was caught at no tape: the oracle is blind");
    std::println!("drawn: restarts draw the first start's values; the planted redraw is caught at {caught} of 40 tapes");
}
