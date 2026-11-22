//! Controls, typed and by text: a caught plant passes, and each way a control can fail is its own
//! refusal with its own message, naming the plant: blocked by the clean run, missed, caught by
//! another invariant. A plant enum whose `ALL` leaves a variant out fails by the enum's name.

use core::fmt;
use std::panic::{AssertUnwindSafe, Location, catch_unwind};

use super::{
    Broke, Broken, ControlsFailed, Expect, NoPlant, Plant, Planted, Plants, Refusal, control,
    control_judged, controls, controls_judged,
};
use crate::check::observed_lines;
use crate::search::Config;
use crate::tape::Tape;

// The typed form.

/// The invariants of the counter's oracle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CounterBroken {
    /// The count went past its bound of 4.
    PastBound { count: u64 },
    /// The code reported a count it should not have: the clean code itself is wrong.
    Reported,
}

impl fmt::Display for CounterBroken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CounterBroken::PastBound { count } => write!(f, "the count {count} is past its bound of 4"),
            CounterBroken::Reported => write!(f, "the code reported every count"),
        }
    }
}

plants! {
    /// The bugs planted in the counter.
    enum CounterPlant breaks CounterBroken {
        /// The counter as it is.
        None,
        /// The bound is not held.
        Unbounded => CounterBroken::PastBound { .. },
    }
}

/// Whether the clean code is itself wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Clean {
    Holds,
    /// It reports every case: a clean run that fails.
    Reports,
}

/// The code under a control, taking its plant through the seam (rule 78), and its oracle.
fn counter(tape: &Tape, plant: impl Planted<CounterPlant>, clean: Clean) -> Result<(), Broken<CounterBroken>> {
    let steps = tape.below(8, "steps");
    let bound = if plant.has(CounterPlant::Unbounded) { u64::MAX } else { 4 };
    let count = steps.min(bound);
    if clean == Clean::Reports {
        return Err(Broken::Oracle(CounterBroken::Reported));
    }
    if count > 4 {
        return Err(Broken::Oracle(CounterBroken::PastBound { count }));
    }
    Ok(())
}

/// `controls_judged` of `P` over `run`, its observe lines given back.
#[track_caller]
fn judged<P: Plant<CounterBroken>>(
    cases: u32,
    run: impl Fn(&Tape, P) -> Result<(), Broken<CounterBroken>>,
) -> (Result<(), ControlsFailed>, String) {
    let mut out = observed_lines();
    let judged = controls_judged(Location::caller(), &Config::cases(cases), &run, &mut out);
    (judged, out.as_str().to_owned())
}

#[test]
fn a_typed_plant_caught_by_its_invariant_passes_and_says_where() {
    let (judged, lines) = judged(64, |tape, plant: CounterPlant| counter(tape, plant, Clean::Holds));
    assert_eq!(judged, Ok(()));
    assert!(lines.starts_with("  plant Unbounded: caught at case "), "{lines}");
}

#[test]
fn the_public_entry_point_passes_a_caught_plant() {
    controls(Config::cases(64), |tape, plant: CounterPlant| counter(tape, plant, Clean::Holds));
}

#[test]
fn the_shipped_seam_plants_nothing_and_a_plant_plants_itself() {
    assert!(!NoPlant.has(CounterPlant::Unbounded));
    assert!(!NoPlant.has(CounterPlant::None));
    assert!(CounterPlant::Unbounded.has(CounterPlant::Unbounded));
    assert!(!CounterPlant::None.has(CounterPlant::Unbounded));
    // The shipped instantiation runs clean.
    crate::check(Config::cases(64), |tape| counter(tape, NoPlant, Clean::Holds));
}

#[test]
fn the_macro_names_each_plant_and_its_invariant_by_its_pattern() {
    assert_eq!(<CounterPlant as Plant<CounterBroken>>::ALL, &[CounterPlant::Unbounded]);
    assert_eq!(<CounterPlant as Plant<CounterBroken>>::NONE, CounterPlant::None);
    assert_eq!(CounterPlant::Unbounded.name(), "Unbounded");
    let expect = CounterPlant::Unbounded.breaks();
    assert!(expect.name().contains("PastBound"), "{}", expect.name());
    assert!(expect.holds(&CounterBroken::PastBound { count: 5 }));
    assert!(!expect.holds(&CounterBroken::Reported));
}

/// (i) The clean run fails: every plant is BLOCKED, not missed, and the message names the plant.
#[test]
fn a_typed_control_whose_clean_run_fails_is_blocked_naming_the_plant() {
    let (judged, _) = judged(64, |tape, plant: CounterPlant| counter(tape, plant, Clean::Reports));
    let failed = judged.expect_err("a control whose clean run fails is refused");
    assert!(failed.blocked(), "{failed}");
    assert_eq!(failed.refusal(0), Some(Refusal::Blocked));
    assert!(
        failed.message.starts_with(
            "blaze-sim control blocked: control Unbounded: the clean run fails, so this control proves nothing: blaze-sim: `"
        ),
        "{failed}"
    );
    assert!(failed.message.contains("the code reported every count"), "{failed}");
    assert!(failed.message.contains("the clean run's report:\n"), "{failed}");
}

/// The public entry point unwinds with the typed payload, so code in the process tells a blocked
/// control from a missed one by type, not by words.
#[test]
fn a_blocked_control_unwinds_with_a_payload_that_says_blocked() {
    let payload = catch_unwind(AssertUnwindSafe(|| {
        controls(Config::cases(16), |tape, plant: CounterPlant| counter(tape, plant, Clean::Reports));
    }))
    .expect_err("a blocked control fails its test");
    let failed = payload
        .downcast_ref::<ControlsFailed>()
        .unwrap_or_else(|| panic!("a control's payload is a `ControlsFailed`"));
    assert!(failed.blocked());
}

plants! {
    /// Plants that name the wrong invariant, or plant nothing.
    enum Misnamed breaks CounterBroken {
        /// The counter as it is.
        None,
        /// The bound is not held, but the plant names `Reported`.
        Unbounded => CounterBroken::Reported,
        /// Plants nothing.
        Inert => CounterBroken::PastBound { .. },
    }
}

/// (ii) and (iii) in one run: one plant caught by another invariant than the one it names, one
/// not caught at all; each refusal names its plant, and both are judged before the test fails.
#[test]
fn a_plant_caught_by_another_invariant_and_one_missed_are_each_refused_by_name() {
    let (judged, _) = judged(64, |tape, plant: Misnamed| {
        let planted = match plant {
            Misnamed::Unbounded => CounterPlant::Unbounded,
            Misnamed::None | Misnamed::Inert => CounterPlant::None,
        };
        counter(tape, planted, Clean::Holds)
    });
    let failed = judged.expect_err("a plant caught by the wrong invariant is refused");
    assert!(!failed.blocked());
    assert_eq!(failed.plants(), 2);
    assert_eq!(failed.refusal(0), Some(Refusal::OtherInvariant));
    assert_eq!(failed.refusal(1), Some(Refusal::Missed));
    let mut lines = failed.message.lines();
    let other = lines.next().unwrap_or("");
    assert!(
        other.starts_with(
            "blaze-sim control other-invariant: control Unbounded: caught, but by `the count "
        ),
        "{other}"
    );
    assert!(other.contains(" is past its bound of 4`, not `"), "{other}");
    assert!(other.contains("Reported` ("), "{other}");
    assert_eq!(
        lines.next(),
        Some("blaze-sim control missed: control Inert: the planted bug was not caught in 64 cases")
    );
}

/// A hand-written `Plant` impl whose `ALL` leaves a variant out: the forgotten plant would have no
/// control, so the test fails by the enum's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HandPlant {
    None,
    Listed,
    Forgotten,
}

impl Plants for HandPlant {}

impl Plant<CounterBroken> for HandPlant {
    const NONE: HandPlant = HandPlant::None;
    const ALL: &'static [HandPlant] = &[HandPlant::Listed];

    fn name(self) -> &'static str {
        match self {
            HandPlant::None => "None",
            HandPlant::Listed => "Listed",
            HandPlant::Forgotten => "Forgotten",
        }
    }

    fn breaks(self) -> Expect<CounterBroken> {
        Expect::new("CounterBroken::PastBound { .. }", |broken| {
            matches!(broken, CounterBroken::PastBound { .. })
        })
    }
}

#[test]
fn a_plant_variant_left_out_of_all_fails_naming_the_enum() {
    let payload = catch_unwind(AssertUnwindSafe(|| {
        let _ = judged(16, |tape, plant: HandPlant| {
            let planted = if plant == HandPlant::Listed { CounterPlant::Unbounded } else { CounterPlant::None };
            counter(tape, planted, Clean::Holds)
        });
    }))
    .expect_err("a plant left out of `ALL` fails the test");
    let said = payload
        .downcast_ref::<String>()
        .cloned()
        .unwrap_or_else(|| panic!("the refusal of a malformed enum is a message"));
    assert!(said.contains("HandPlant` has 3 variants, but `ALL` lists 1 plants"), "{said}");
    assert_eq!(HandPlant::Forgotten.name(), "Forgotten", "the forgotten variant is a real one");
}

/// A plant enum as a crate ships it: declared beside the code that takes it, with only the seam's
/// `Plants`; the tests then implement `Plant` over it with `plants! { impl .. }`, no second enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShippedPlant {
    None,
    Unbounded,
}

impl Plants for ShippedPlant {}

plants! {
    impl ShippedPlant breaks CounterBroken {
        None,
        Unbounded => CounterBroken::PastBound { .. },
    }
}

/// The counter, taking the shipped enum through the seam.
fn shipped_counter(tape: &Tape, plant: impl Planted<ShippedPlant>) -> Result<(), Broken<CounterBroken>> {
    let planted = if plant.has(ShippedPlant::Unbounded) { CounterPlant::Unbounded } else { CounterPlant::None };
    counter(tape, planted, Clean::Holds)
}

#[test]
fn the_impl_form_makes_a_shipped_enum_a_plant_and_its_control_passes() {
    assert_eq!(<ShippedPlant as Plant<CounterBroken>>::ALL, &[ShippedPlant::Unbounded]);
    assert_eq!(<ShippedPlant as Plant<CounterBroken>>::NONE, ShippedPlant::None);
    assert_eq!(ShippedPlant::Unbounded.name(), "Unbounded");
    assert!(ShippedPlant::Unbounded.breaks().holds(&CounterBroken::PastBound { count: 5 }));
    assert!(!ShippedPlant::Unbounded.breaks().holds(&CounterBroken::Reported));
    controls(Config::cases(64), |tape, plant: ShippedPlant| shipped_counter(tape, plant));
}

// The text form, deprecated, kept while callers move.

/// The code under a text control: a counter that a planted bug lets go past its bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bug {
    None,
    /// The bound is not held.
    Unbounded,
    /// The clean code is itself wrong: it reports every case.
    AlwaysReports,
}

fn counter_by_text(tape: &Tape, bug: Bug) -> Result<(), Broke> {
    let steps = tape.below(8, "steps");
    let bound = match bug {
        Bug::Unbounded => u64::MAX,
        Bug::None | Bug::AlwaysReports => 4,
    };
    let count = steps.min(bound);
    if bug == Bug::AlwaysReports || count > 4 {
        return Err(Broke::Oracle(format!("the count {count} is past its bound of 4")));
    }
    Ok(())
}

/// `control_judged` of one plant by text.
#[track_caller]
fn judged_by_text(
    clean: Bug,
    plant: Bug,
    says: &str,
    run: impl Fn(&Tape, Bug) -> Result<(), Broke>,
) -> Result<(), ControlsFailed> {
    let mut out = observed_lines();
    control_judged(Location::caller(), Config::cases(64), clean, plant, says, run, &mut out)
}

#[test]
fn a_plant_caught_saying_the_words_passes() {
    control(Config::cases(64), Bug::None, Bug::Unbounded, "past its bound", counter_by_text);
}

#[test]
fn an_oracle_that_misses_the_plant_is_refused_as_missed() {
    let failed = judged_by_text(Bug::None, Bug::None, "past its bound", counter_by_text)
        .expect_err("a control whose plant is the clean code is refused");
    assert_eq!(failed.refusal(0), Some(Refusal::Missed));
    assert_eq!(
        failed.message,
        "blaze-sim control missed: control None: the planted bug was not caught in 64 cases\n"
    );
}

#[test]
fn a_plant_caught_with_other_words_is_refused_quoting_them() {
    let failed = judged_by_text(Bug::None, Bug::Unbounded, "lost a message", counter_by_text)
        .expect_err("a control caught with other words is refused");
    assert_eq!(failed.refusal(0), Some(Refusal::OtherInvariant));
    let said = failed.message.as_str();
    assert!(
        said.starts_with(
            "blaze-sim control other-invariant: control Unbounded: caught, but by `the count "
        ),
        "{said}"
    );
    assert!(said.contains(" is past its bound of 4`, not `lost a message` ("), "{said}");
    assert!(said.ends_with(" of 64 cases tripped another invariant, none `lost a message`)\n"), "{said}");
}

/// A plant that trips two of an oracle's checks: check A on the first ten calls, check B on a
/// later call that draws more than 4 steps. Clean, it trips neither.
fn two_checks(calls: &core::cell::Cell<u32>) -> impl Fn(&Tape, Bug) -> Result<(), Broke> + '_ {
    move |tape: &Tape, bug: Bug| {
        let steps = tape.below(8, "steps");
        if bug == Bug::None {
            return Ok(());
        }
        calls.set(calls.get() + 1);
        let call = calls.get();
        match (call <= 10, steps > 4) {
            (true, _) => Err(Broke::Oracle(format!("check A on call {call}"))),
            (false, true) => Err(Broke::Oracle(format!("check B on call {call}"))),
            (false, false) => Ok(()),
        }
    }
}

#[test]
fn a_plant_caught_by_another_check_first_passes_a_control_naming_the_later_one() {
    let calls = core::cell::Cell::new(0);
    control(Config::cases(64), Bug::None, Bug::Unbounded, "check B", two_checks(&calls));
    assert!(calls.get() > 10, "the planted run stopped before check B could trip");
}

#[test]
fn a_control_whose_words_no_case_says_is_refused_with_the_count_and_the_first_other_text() {
    let calls = core::cell::Cell::new(0);
    let failed = judged_by_text(Bug::None, Bug::Unbounded, "check C", two_checks(&calls))
        .expect_err("a control whose words no case says is refused");
    assert_eq!(failed.refusal(0), Some(Refusal::OtherInvariant));
    let said = failed.message.as_str();
    let count: u32 = said
        .strip_prefix(
            "blaze-sim control other-invariant: control Unbounded: caught, but by `check A on call 1`, not `check C` (",
        )
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or_else(|| panic!("no count in `{said}`"))
        .parse()
        .unwrap_or_else(|error| panic!("the count in `{said}` is not a number: {error}"));
    assert!(count > 10 && count <= 64, "{said}");
}

#[test]
fn a_plant_caught_by_a_panic_is_not_the_oracle_catching_it() {
    let failed = judged_by_text(Bug::None, Bug::Unbounded, "past its bound", |tape, bug| {
        let steps = tape.below(8, "steps");
        if bug == Bug::Unbounded && steps > 4 {
            panic!("the count is past its bound");
        }
        Ok(())
    })
    .expect_err("a plant caught by a panic, not the oracle, is refused");
    assert_eq!(failed.refusal(0), Some(Refusal::OtherInvariant));
    let said = failed.message.as_str();
    assert!(
        said.starts_with("blaze-sim control other-invariant: control Unbounded: caught, but by a panic at "),
        "{said}"
    );
    assert!(said.contains(": `the count is past its bound`, not `past its bound`"), "{said}");
}

/// (i) for the text form too: the clean run's failure names the plant and is BLOCKED.
#[test]
fn a_text_control_whose_clean_run_fails_is_blocked_naming_the_plant() {
    let failed = judged_by_text(Bug::AlwaysReports, Bug::Unbounded, "past its bound", counter_by_text)
        .expect_err("a control whose clean run fails is refused");
    assert!(failed.blocked());
    let said = failed.message.as_str();
    assert!(
        said.starts_with(
            "blaze-sim control blocked: control Unbounded: the clean run fails, so this control proves nothing: blaze-sim: `"
        ),
        "{said}"
    );
    assert!(
        said.contains("failed at seed 0 (case 1 of 64): the count 0 is past its bound of 4"),
        "{said}"
    );
    assert!(said.contains("replay: BLAZE_SIM_TAPE="), "{said}");
}

#[test]
fn a_text_control_unwinds_with_the_typed_payload() {
    let payload = catch_unwind(AssertUnwindSafe(|| {
        control(Config::cases(16), Bug::None, Bug::None, "past its bound", counter_by_text);
    }))
    .expect_err("a missed plant fails its test");
    let failed = payload
        .downcast_ref::<ControlsFailed>()
        .unwrap_or_else(|| panic!("a control's payload is a `ControlsFailed`"));
    assert_eq!(failed.refusal(0), Some(Refusal::Missed));
    assert!(!failed.blocked());
}
