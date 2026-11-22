//! Vacuity and power in observe mode (observe.rs; test.md piece S4): the tally of what passing
//! cases drew, the block it prints, and `control`'s lines for each plant. Each test prints the
//! block it judged; its doc holds that block (the case counts in angle brackets are the seeds'
//! own, to verify by the run).

#![expect(
    blaze::error_debug,
    reason = "a test's failure message shows the values it compared as they are"
)]

use core::cell::Cell;
use core::panic::Location;

use crate::check::{observed_lines, search_observing};
use crate::control::{control, control_observed};
use crate::observe::{self, ObserveMode, Tally, TallyPlant};
use crate::search::{Broke, Config, Search, Verdict};
use crate::tape::{Choice, Record, Tape};

/// The lines of a search of `config` over `body`, which has to pass.
fn lines_of(config: Config, body: impl FnMut(&Tape)) -> (Lines, u32) {
    let (verdict, searched) = search_observing(config, body);
    assert!(matches!(verdict, Verdict::Passed(_)), "the search did not pass: {verdict:?}");
    let mut lines = observed_lines();
    searched.tally().lines_into(&mut lines);
    (lines, searched.tally().cases())
}

/// The driver's buffer for observe lines.
type Lines = blaze_own::String<std::alloc::Global>;

/// The block of `lines` at `place`: empty when there are no lines.
fn block_of(place: &str, lines: &str) -> Lines {
    let mut block = observed_lines();
    observe::block(place, lines, &mut block);
    block
}

/// (a) `n` is drawn from ten values, but `assume` lets only 3 through: every passing case drew
/// the same value, so the test never exercised the other nine. Prints:
///
/// ```text
/// blaze-sim observe: n
///   `n` never varied (<about 100> cases, always 3)
/// ```
#[test]
fn a_draw_that_an_assume_pins_is_listed_as_never_varied() {
    let (lines, cases) = lines_of(Config::cases(1000), |tape| {
        let n = tape.below(10, "n");
        tape.assume(n == 3);
    });
    println!("{}", block_of("n", &lines));
    assert!(cases >= observe::LEAST_CASES, "only {cases} of 1000 cases passed");
    assert_eq!(lines.as_str(), format!("  `n` never varied ({cases} cases, always 3)\n").as_str());
}

/// (a) The same draw with nothing assumed takes many values: nothing is listed, and no block is
/// printed. Prints: `no block`.
#[test]
fn a_draw_that_varies_is_not_listed() {
    let (lines, cases) = lines_of(Config::cases(1000), |tape| {
        let _ = tape.below(10, "n");
    });
    assert_eq!(cases, 1000);
    assert_eq!(lines.as_str(), "", "a draw that varied was listed");
    assert_eq!(block_of("n", &lines).as_str(), "", "a block was made of no lines");
    println!("no block");
}

/// The oracle for (b): `cases` passing cases each draw `n` once, under bound `most` (at least 1),
/// and the tally says "never varied" exactly when every case drew the same value.
fn the_tally_says_never_varied_rightly(tape: &Tape, plant: TallyPlant) -> Result<(), Broke> {
    let cases = u64::from(observe::LEAST_CASES) + tape.below(33, "cases");
    let most = 1 + tape.below(9, "most");
    let mut tally = Tally::planted(plant);
    let mut record = Record::new();
    let mut first: Option<u64> = None;
    let mut other: Option<u64> = None;
    for _ in 0..cases {
        let value = tape.below(most + 1, "value");
        match first {
            None => first = Some(value),
            Some(first) if first != value => other = Some(value),
            Some(_) => {}
        }
        record.choices.clear();
        record.choices.push(Choice { value, most, label: "n" });
        tally.case(&record);
    }
    let mut lines = observed_lines();
    tally.lines_into(&mut lines);
    let said = lines.contains("never varied");
    match (said, first, other) {
        (true, Some(first), Some(other)) => Err(Broke::Oracle(format!(
            "the tally said `n` never varied, but it took {first} and {other} in {cases} cases"
        ))),
        (false, Some(first), None) => Err(Broke::Oracle(format!(
            "the tally missed that `n` was {first} in all {cases} cases: `{lines}`"
        ))),
        (true, Some(_), None) | (false, Some(_), Some(_)) => Ok(()),
        (_, None, _) => Err(Broke::Harness(format!("no case drew `n` of {cases} cases"))),
    }
}

/// (b) The planted bug: a tally that counts a label's values within each case, not across the
/// cases, says "never varied" of a draw that took many values; the oracle catches it, and the
/// tally as it is passes. Prints (on standard error, `control`'s block):
///
/// ```text
/// blaze-sim observe: <this test's name>
///   plant PerCase: caught at case 1 of 200
/// ```
#[test]
fn control_a_tally_counting_values_per_case_is_caught() {
    control(
        Config::cases(200),
        TallyPlant::None,
        TallyPlant::PerCase,
        "said `n` never varied",
        the_tally_says_never_varied_rightly,
    );
}

/// The plants of (c): one every case catches, one only the last case of the run catches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bug {
    None,
    Early,
    Late,
}

/// The cases of (c).
const CONTROL_CASES: u32 = 20;

/// (c) `control` over two plants: `Early` is caught at the first case; `Late` only at the last
/// of the 20, the end of the budget, so it is also listed as weak. Prints:
///
/// ```text
/// blaze-sim observe: early
///   plant Early: caught at case 1 of 20
/// blaze-sim observe: late
///   plant Late: caught at case 20 of 20
///   weak: plant Late needed 20 of 20 cases
/// ```
#[test]
fn control_lines_say_where_each_plant_was_caught_and_a_late_one_is_weak() {
    let late_runs = Cell::new(0u32);
    let run = |tape: &Tape, bug: Bug| -> Result<(), Broke> {
        let steps = tape.below(8, "steps");
        let caught = match bug {
            Bug::None => false,
            Bug::Early => true,
            Bug::Late => {
                late_runs.set(late_runs.get() + 1);
                late_runs.get() == CONTROL_CASES
            }
        };
        if caught {
            return Err(Broke::Oracle(format!("the count {steps} is past its bound")));
        }
        Ok(())
    };
    let at = Location::caller();
    let config = Config::cases(CONTROL_CASES);
    let mut early = observed_lines();
    control_observed(at, config.clone(), Bug::None, Bug::Early, "past its bound", run, &mut early);
    let mut late = observed_lines();
    control_observed(at, config, Bug::None, Bug::Late, "past its bound", run, &mut late);
    println!("{}", block_of("early", &early));
    println!("{}", block_of("late", &late));
    assert_eq!(early.as_str(), "  plant Early: caught at case 1 of 20\n");
    assert_eq!(
        late.as_str(),
        "  plant Late: caught at case 20 of 20\n  weak: plant Late needed 20 of 20 cases\n"
    );
    assert!(!observe::Caught { case: 18, cases: 20 }.weak(), "case 18 of 20 is not the last tenth");
    assert!(observe::Caught { case: 19, cases: 20 }.weak(), "case 19 of 20 is the last tenth");
}

/// Two labels pinned by `assume`, drawn in the order `zeta`, `alpha`, and one that varies.
fn pinned(tape: &Tape) {
    let zeta = tape.below(4, "zeta");
    let alpha = tape.below(4, "alpha");
    tape.assume(zeta == 1 && alpha == 2);
    let _ = tape.below(4, "mid");
}

/// (d) The block for one config and seed is the same bytes on a second run, and lists its labels
/// sorted (`alpha` before `zeta`, whatever order they were drawn in). Prints:
///
/// ```text
/// blaze-sim observe: pinned
///   `alpha` never varied (<about 125> cases, always 2)
///   `zeta` never varied (<the same count> cases, always 1)
/// ```
#[test]
fn the_block_is_equal_across_runs_and_its_labels_sorted() {
    let config = Config::cases(2000).first(7);
    let (first, cases) = lines_of(config.clone(), pinned);
    let (second, _) = lines_of(config, pinned);
    let first = block_of("pinned", &first);
    let second = block_of("pinned", &second);
    println!("{first}");
    assert_eq!(first.as_bytes(), second.as_bytes(), "two runs of one seed gave two blocks");
    assert_eq!(
        first.as_str(),
        format!(
            "blaze-sim observe: pinned\n  `alpha` never varied ({cases} cases, always 2)\n  `zeta` never varied ({cases} cases, always 1)\n"
        )
        .as_str()
    );
}

/// (e) `Enforce` is built (D165): the search observes under it as under `Observe`, and the
/// "never varied" lines it fails on are exactly the ones the block lists. (The driver failing a
/// run on them, and `search` refusing it, are in tests/limits.rs.)
#[test]
fn enforce_fails_on_exactly_the_never_varied_lines() {
    let search = Search::new(Config::cases(1).observe(ObserveMode::Enforce));
    assert_eq!(search.tally().cases(), 0);
    let (lines, _) = lines_of(Config::cases(1000), |tape| {
        let n = tape.below(10, "n");
        tape.assume(n == 3);
        let _ = tape.below(10, "free");
    });
    let (verdict, searched) = search_observing(Config::cases(1000), |tape| {
        let n = tape.below(10, "n");
        tape.assume(n == 3);
        let _ = tape.below(10, "free");
    });
    assert!(matches!(verdict, Verdict::Passed(_)), "{verdict:?}");
    let mut enforced = observed_lines();
    assert_eq!(searched.tally().never_varied_into(&mut enforced), 1);
    assert_eq!(enforced.as_str(), lines.as_str());
}
