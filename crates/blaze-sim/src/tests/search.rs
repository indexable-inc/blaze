//! The search and the shrinker, tested with the engine itself: a test draws its parameters with
//! `crate::check`, runs `crate::search` over an inner body and judges the verdict.

#![expect(
    blaze::error_debug,
    reason = "a test's failure message shows the values it compared as they are"
)]

use std::cell::Cell;
use std::collections::BTreeSet;

use blaze_rng::SplitMix64;

use crate::check::{search, search_planted};
use crate::search::{
    Broke, Config, Ended, Failure, Given, GivenError, Next, Origin, Passed, Phase, Plant, SEED,
    SEEDS, Search, TAPE, Verdict,
};
use crate::shrink::{Shrinker, simpler};
use crate::tape::{Record, Tail, Tape};

/// Family (a): 20 draws below 100; fails when one is at least `t`. Its least failing tape is
/// the single value `t`.
fn at_least(tape: &Tape, t: u64) -> Result<(), String> {
    let mut over = None;
    for at in 0..20 {
        let x = tape.below(100, "x");
        if x >= t && over.is_none() {
            over = Some((at, x));
        }
    }
    match over {
        Some((at, x)) => Err(format!("x[{at}] = {x} is at least {t}")),
        None => Ok(()),
    }
}

/// Family (b): a list of up to 12 items below 10; fails when at least `k` are nonzero. Its
/// least failing tape is `k` elements each 1, each written as `more = 1` then the item.
fn nonzero_items(tape: &Tape, k: usize) -> Result<(), String> {
    let items = tape.vec(0..=12, "items", |tape| tape.below(10, "item"));
    let nonzero = items.iter().filter(|&&item| item != 0).count();
    match nonzero >= k {
        true => Err(format!("{nonzero} of {items:?} are nonzero, at least {k}")),
        false => Ok(()),
    }
}

/// Family (c): 6 slots below 8; fails when at least `m` are nonzero and some slot is larger
/// than the next. Its least failing tape is `m` ones for `m < 6` (the zero after them is the
/// descent), and `1 1 1 1 2 1` for `m == 6`.
fn unsorted(tape: &Tape, m: usize) -> Result<(), String> {
    let slots: [u64; 6] = core::array::from_fn(|_| tape.below(8, "slot"));
    let nonzero = slots.iter().filter(|&&slot| slot != 0).count();
    let descent = slots.windows(2).any(|pair| pair[0] > pair[1]);
    match nonzero >= m && descent {
        true => Err(format!("{slots:?} has {nonzero} nonzero slots and a descent")),
        false => Ok(()),
    }
}

fn least_unsorted(m: usize) -> Vec<u64> {
    match m < 6 {
        true => vec![1; m],
        false => vec![1, 1, 1, 1, 2, 1],
    }
}

/// The values a verdict's least tape holds, trailing zeros cut, or why there is none.
fn least_of(verdict: &Verdict) -> Result<Vec<u64>, String> {
    let Verdict::Failed(failed) = verdict else {
        return Err(format!("the search did not end `Failed`: {verdict:?}"));
    };
    let mut values = Vec::new();
    failed.least.values_into(&mut values);
    Ok(values)
}

/// Whether a search (with `plant`) over `body` ends on exactly `least`.
fn ends_on(
    plant: Plant,
    body: impl FnMut(&Tape) -> Result<(), String>,
    least: &[u64],
) -> Result<(), String> {
    let verdict = search_planted(Config::DEFAULT, plant, body);
    let values = least_of(&verdict)?;
    match values == least {
        true => Ok(()),
        false => {
            Err(format!("the search ended on {values:?}, not the least failing tape {least:?}"))
        }
    }
}

/// The invariant: a search over a body with a known least failing tape ends on exactly that tape.
#[test]
fn a_search_ends_on_the_least_failing_tape() {
    crate::check(Config::cases(12), |tape| {
        let t = tape.range(1..100, "t");
        let k = usize::try_from(tape.range(1..6, "k")).expect("k is drawn below 6");
        let m = usize::try_from(tape.range(1..7, "m")).expect("m is drawn below 7");
        ends_on(Plant::None, |inner| at_least(inner, t), &[t])?;
        ends_on(Plant::None, |inner| nonzero_items(inner, k), &vec![1u64; 2 * k])?;
        ends_on(Plant::None, |inner| unsorted(inner, m), &least_unsorted(m))
    });
}

#[test]
fn the_oracle_catches_a_shrinker_that_keeps_a_passing_candidate() {
    let caught = ends_on(Plant::KeepsPassing, |inner| at_least(inner, 10), &[10]);
    assert!(
        caught.is_err(),
        "a shrinker that keeps passing candidates still ended on the least tape"
    );
}

#[test]
fn the_oracle_catches_a_shrinker_that_keeps_a_record_that_is_not_simpler() {
    let caught = ends_on(Plant::KeepsNotSimpler, |inner| at_least(inner, 10), &[10]);
    assert!(
        caught.is_err(),
        "a shrinker that keeps records that are not simpler still ended on the least tape"
    );
}

/// Whether `config` over `body` gives one verdict, run twice.
fn same_twice<O: crate::Outcome>(
    config: Config,
    mut body: impl FnMut(&Tape) -> O,
) -> Result<(), String> {
    let first = search(config.clone(), &mut body);
    let again = search(config, &mut body);
    if first != again {
        return Err(format!("one config gave two verdicts:\n{first:?}\n{again:?}"));
    }
    Ok(())
}

/// The invariant: one configuration gives one verdict (the same search twice gives equal
/// verdicts, for failing and passing bodies).
#[test]
fn one_configuration_gives_one_verdict() {
    crate::check(Config::cases(8), |tape| {
        let t = tape.range(1..100, "t");
        let first = tape.below(1000, "first");
        same_twice(Config::DEFAULT.first(first), |inner| at_least(inner, t))?;
        same_twice(Config::cases(64).first(first), |inner: &Tape| {
            inner.below(10, "x");
        })
    });
}

#[test]
fn the_oracle_catches_a_verdict_that_depends_on_a_counter() {
    let calls = Cell::new(0u32);
    let body = |tape: &Tape| -> Result<(), String> {
        calls.set(calls.get() + 1);
        tape.below(10, "x");
        match calls.get() == 3 {
            true => Err("the third call".to_owned()),
            false => Ok(()),
        }
    };
    assert!(
        same_twice(Config::DEFAULT, body).is_err(),
        "a verdict that depends on a counter was given twice"
    );
}

/// Whether the least tape of a search (with `plant`) over family (a), given back as a tape,
/// fails with the same origin and an equal record.
fn replays_exactly(plant: Plant, t: u64) -> Result<(), String> {
    let found = search_planted(Config::DEFAULT, plant, |inner| at_least(inner, t));
    let Verdict::Failed(failed) = &found else {
        return Err(format!("the search did not end `Failed`: {found:?}"));
    };
    let values = least_of(&found)?;
    let config = Config::DEFAULT.given(Given { seed: None, seeds: None, tape: Some(values) });
    let replay = search(config, |inner| at_least(inner, t));
    let Verdict::Failed(again) = &replay else {
        return Err(format!("the least tape, given back, did not fail: {replay:?}"));
    };
    if again.failure.origin != failed.failure.origin {
        return Err(format!(
            "the replay failed from {:?}, not {:?}",
            again.failure.origin, failed.failure.origin
        ));
    }
    match again.least == failed.least {
        true => Ok(()),
        false => Err(format!("the replay recorded {:?}, not {:?}", again.least, failed.least)),
    }
}

/// The invariant: the least tape of a `Verdict::Failed` verdict, given back as `Given.tape`, fails with
/// the same origin and an equal record (replay is exact).
#[test]
fn the_least_tape_replays_exactly() {
    crate::check(Config::cases(12), |tape| replays_exactly(Plant::None, tape.range(1..100, "t")));
}

#[test]
fn the_oracle_catches_a_search_that_reports_the_candidate_as_proposed() {
    assert!(
        replays_exactly(Plant::ReportsProposed, 10).is_err(),
        "a least tape reported as proposed replayed exactly"
    );
}

/// Whether a body whose failure depends on a flag outside its tape ends
/// `Verdict::NotAFunction`.
fn reports_outside_reads(plant: Plant) -> Result<(), String> {
    let failed_once = Cell::new(false);
    let verdict = search_planted(Config::DEFAULT, plant, |tape: &Tape| -> Result<(), String> {
        let x = tape.below(10, "x");
        match x >= 1 && !failed_once.get() {
            true => {
                failed_once.set(true);
                Err(format!("x = {x}, the first time"))
            }
            false => Ok(()),
        }
    });
    match verdict {
        Verdict::NotAFunction { seed: Some(_), again: Ended::Passed, .. } => Ok(()),
        other @ (Verdict::Passed(..)
        | Verdict::Failed(..)
        | Verdict::NotAFunction { .. }
        | Verdict::DrewNothing { .. }
        | Verdict::AllDiscarded { .. }
        | Verdict::Unreached { .. }
        | Verdict::Misfit { .. }) => {
            Err(format!("a body that reads outside its tape ended {other:?}"))
        }
    }
}

/// The invariant: a body whose failure depends on something outside its tape ends
/// `Verdict::NotAFunction`.
#[test]
fn a_body_that_reads_outside_its_tape_is_not_a_function() {
    reports_outside_reads(Plant::None).unwrap();
}

#[test]
fn the_oracle_catches_a_search_that_skips_the_final_run() {
    assert!(
        reports_outside_reads(Plant::SkipsFinalRun).is_err(),
        "a search with no final run still said `NotAFunction`"
    );
}

#[test]
fn a_body_that_draws_nothing_ends_drew_nothing() {
    assert_eq!(search(Config::cases(10), |_tape: &Tape| {}), Verdict::DrewNothing { cases: 10 });
}

#[test]
fn a_body_that_always_discards_ends_all_discarded() {
    let verdict = search(Config::cases(10), |tape: &Tape| {
        if tape.below(3, "x") < 3 {
            tape.discard()
        }
    });
    assert_eq!(verdict, Verdict::AllDiscarded { cases: 10 });
}

#[test]
fn a_missing_reach_point_ends_unreached_and_a_reached_one_passes() {
    let body = |tape: &Tape| {
        if tape.coin("go") {
            tape.reached("near");
        }
    };
    let verdict = search(Config::cases(20).must_reach(&["far", "near"]), body);
    assert_eq!(verdict, Verdict::Unreached { cases: 20, missing: vec!["far"] });
    let verdict = search(Config::cases(20).must_reach(&["near"]), body);
    assert_eq!(verdict, Verdict::Passed(Passed { cases: 20, discarded: 0, choices: 20 }));
}

/// Runs a search with a body that returns, noting each proposal's phase.
fn phases(
    mut search: Search,
    mut body: impl FnMut(&Tape) -> Result<(), String>,
) -> (Verdict, Vec<Phase>) {
    let tape = Tape::new(&[], Tail::Calm, Tape::MOST_CHOICES);
    let mut record = Record::new();
    let mut phases = Vec::new();
    for _ in 0..10_000 {
        let proposal = match search.next() {
            Next::Done(
                verdict @ (Verdict::Passed(..)
                | Verdict::Failed(..)
                | Verdict::NotAFunction { .. }
                | Verdict::DrewNothing { .. }
                | Verdict::AllDiscarded { .. }
                | Verdict::Unreached { .. }
                | Verdict::Misfit { .. }),
            ) => return (verdict.clone(), phases),
            Next::Run(proposal) => proposal,
        };
        phases.push(proposal.phase);
        tape.reset(proposal.prefix, proposal.tail);
        let ended = match body(&tape) {
            Ok(()) => Ended::Passed,
            Err(text) => Ended::Failed(Failure { origin: Origin::Error, text }),
        };
        tape.finish(&mut record);
        search.ran(&record, ended);
    }
    panic!("the search did not end in 10,000 runs");
}

#[test]
fn a_given_seed_runs_alone_and_is_shrunk() {
    let config = Config::DEFAULT.given(Given { seed: Some(7), seeds: None, tape: None });
    let (verdict, phases) = phases(Search::new(config), |tape| at_least(tape, 1));
    assert_eq!(phases.iter().filter(|&&phase| phase == Phase::Generate).count(), 1, "{phases:?}");
    assert_eq!(phases[0], Phase::Generate);
    assert!(phases.contains(&Phase::Shrink), "{phases:?}");
    assert_eq!(phases.last(), Some(&Phase::Final));
    let Verdict::Failed(failed) = &verdict else { panic!("{verdict:?}") };
    assert_eq!((failed.seed, failed.case, failed.cases), (Some(7), 1, 1));
    assert_eq!(least_of(&verdict).unwrap(), vec![1]);
}

#[test]
fn a_given_tape_runs_alone_with_no_shrinking() {
    let config = Config::DEFAULT.given(Given { seed: None, seeds: None, tape: Some(vec![0, 5]) });
    let (verdict, phases) = phases(Search::new(config), |tape| at_least(tape, 1));
    assert_eq!(phases, vec![Phase::Given]);
    let Verdict::Failed(failed) = &verdict else { panic!("{verdict:?}") };
    assert_eq!((failed.seed, failed.case, failed.cases, failed.shrink_runs), (None, 1, 1, 0));
    assert_eq!(least_of(&verdict).unwrap(), vec![0, 5]);
}

#[test]
fn a_given_tape_that_does_not_fit_ends_misfit() {
    let too_large =
        Config::DEFAULT.given(Given { seed: None, seeds: None, tape: Some(vec![0, 150]) });
    let verdict = search(too_large, |tape| at_least(tape, 1));
    assert!(
        matches!(verdict, Verdict::Misfit { clamped: 1, unused: 0, ended: Ended::Failed(_) }),
        "{verdict:?}"
    );
    let never_drawn =
        Config::DEFAULT.given(Given { seed: None, seeds: None, tape: Some(vec![3, 1, 2]) });
    let verdict = search(never_drawn, |tape: &Tape| {
        tape.below(10, "x");
    });
    assert_eq!(verdict, Verdict::Misfit { clamped: 0, unused: 2, ended: Ended::Passed });
}

#[test]
fn a_body_that_draws_without_end_fails_as_overran() {
    let verdict = search(Config::cases(4).most_choices(100), |tape: &Tape| {
        for _ in 0..u32::MAX {
            tape.below(2, "forever");
        }
    });
    let Verdict::Failed(failed) = &verdict else { panic!("{verdict:?}") };
    assert_eq!(failed.failure.origin, Origin::Overran);
    assert!(failed.least.overran);
    assert_eq!(failed.least.choices.len(), 100);
}

#[test]
fn the_shrinker_never_proposes_a_tape_twice_nor_runs_past_its_budget() {
    let seeded = Tape::new(&[], Tail::Seeded(SplitMix64::new(3)), Tape::MOST_CHOICES);
    assert!(at_least(&seeded, 10).is_err(), "seed 3 has a value of at least 10 in its 20");
    let mut first = Record::new();
    seeded.finish(&mut first);
    for budget in [0, 1, 7, 50, 1000] {
        let mut shrinker = Shrinker::new(&first, budget);
        let mut proposed = BTreeSet::new();
        let mut record = Record::new();
        let mut ended = false;
        for _ in 0..=budget {
            let Some(candidate) = shrinker.next() else {
                ended = true;
                break;
            };
            let candidate = candidate.to_vec();
            assert!(
                simpler(&candidate, shrinker.best()),
                "{candidate:?} is not simpler than {:?}",
                shrinker.best()
            );
            assert!(proposed.insert(candidate.clone()), "{candidate:?} was proposed twice");
            let tape = Tape::replay(&candidate);
            let same = at_least(&tape, 10).is_err();
            tape.finish(&mut record);
            shrinker.ran(&record, same);
        }
        assert!(ended, "the shrinker with a budget of {budget} proposed past it");
        assert!(shrinker.runs() <= budget, "{} runs over a budget of {budget}", shrinker.runs());
        assert_eq!(shrinker.runs() as usize, proposed.len());
    }
}

/// Whether `simpler` is a strict order on `a`, `b` and `c` that does not see trailing zeros.
fn simpler_is_strict(
    simpler: fn(&[u64], &[u64]) -> bool,
    a: &[u64],
    b: &[u64],
    c: &[u64],
) -> Result<(), String> {
    let tapes = [a, b, c];
    for x in tapes {
        if simpler(x, x) {
            return Err(format!("{x:?} is simpler than itself"));
        }
        let padded: Vec<u64> = x.iter().copied().chain([0, 0]).collect();
        if simpler(&padded, x) || simpler(x, &padded) {
            return Err(format!(
                "{x:?} and {padded:?} differ only in trailing zeros, yet one is simpler"
            ));
        }
        for y in tapes {
            if simpler(&padded, y) != simpler(x, y) {
                return Err(format!(
                    "trailing zeros on {x:?} changed whether it is simpler than {y:?}"
                ));
            }
            for z in tapes {
                if simpler(x, y) && simpler(y, z) && !simpler(x, z) {
                    return Err(format!("{x:?} < {y:?} < {z:?}, yet not {x:?} < {z:?}"));
                }
            }
        }
    }
    Ok(())
}

/// The order on tapes a test checks.
#[derive(Clone, Copy, Debug)]
enum Order {
    /// The crate's own `simpler`.
    Simpler,
    /// The planted bug: an order that counts trailing zeros (shorter raw length first, then the
    /// smaller value at the first difference).
    ByRawLength,
}

/// Whether `a` is shorter than `b` as it is, trailing zeros counted, or of one length and smaller
/// at the first difference.
fn by_raw_length(a: &[u64], b: &[u64]) -> bool {
    a.len() < b.len() || (a.len() == b.len() && a < b)
}

/// Draws three tapes and checks that `order` is a strict order on them blind to trailing zeros.
fn strict(tape: &Tape, order: Order) -> Result<(), Broke> {
    let draw = |tape: &Tape| tape.vec(0..=4, "tape", |tape| tape.below(4, "value"));
    let (a, b, c) = (draw(tape), draw(tape), draw(tape));
    let order: fn(&[u64], &[u64]) -> bool = match order {
        Order::Simpler => simpler,
        Order::ByRawLength => by_raw_length,
    };
    simpler_is_strict(order, &a, &b, &c).map_err(Broke::Oracle)
}

/// The invariant: `simpler` is irreflexive and transitive on drawn triples, and trailing zeros
/// do not change it.
#[test]
fn simpler_is_a_strict_order_blind_to_trailing_zeros() {
    crate::check(Config::cases(200), |tape| strict(tape, Order::Simpler));
}

#[test]
fn the_oracle_catches_a_simpler_that_counts_trailing_zeros() {
    crate::control(
        Config::cases(200),
        Order::Simpler,
        Order::ByRawLength,
        "trailing zeros",
        strict,
    );
}

#[test]
fn given_parse_accepts_and_refuses_naming_the_setting() {
    assert_eq!(Given::parse(None, None, None), Ok(Given::NOTHING));
    assert_eq!(
        Given::parse(Some("41"), None, None),
        Ok(Given { seed: Some(41), seeds: None, tape: None })
    );
    assert_eq!(
        Given::parse(None, Some("1000"), None),
        Ok(Given { seed: None, seeds: Some(1000), tape: None })
    );
    assert_eq!(
        Given::parse(None, None, Some("3 0*2 1")),
        Ok(Given { seed: None, seeds: None, tape: Some(vec![3, 0, 0, 1]) })
    );
    assert_eq!(
        Given::parse(None, None, Some("")),
        Ok(Given { seed: None, seeds: None, tape: Some(Vec::new()) })
    );

    let refusals = [
        (
            Given::parse(Some("x41"), None, None),
            GivenError::NotANumber { setting: SEED, value: "x41".to_owned() },
            SEED,
        ),
        (
            Given::parse(Some("+4"), None, None),
            GivenError::NotANumber { setting: SEED, value: "+4".to_owned() },
            SEED,
        ),
        (
            Given::parse(None, Some("-1"), None),
            GivenError::NotANumber { setting: SEEDS, value: "-1".to_owned() },
            SEEDS,
        ),
        (
            Given::parse(None, Some("4294967296"), None),
            GivenError::NotANumber { setting: SEEDS, value: "4294967296".to_owned() },
            SEEDS,
        ),
        (Given::parse(None, Some("0"), None), GivenError::ZeroSeeds, SEEDS),
        (Given::parse(Some("1"), None, Some("3")), GivenError::TapeWith { setting: SEED }, SEED),
        (Given::parse(None, Some("5"), Some("3")), GivenError::TapeWith { setting: SEEDS }, SEEDS),
        (Given::parse(Some("1"), Some("5"), None), GivenError::SeedWithSeeds, SEEDS),
    ];
    for (parsed, error, setting) in refusals {
        assert_eq!(parsed, Err(error.clone()));
        let said = error.to_string();
        assert!(said.contains(setting), "`{said}` does not name `{setting}`");
    }
    let bad_tape = Given::parse(None, None, Some("3  1"));
    let Err(error @ GivenError::Tape { .. }) = &bad_tape else { panic!("{bad_tape:?}") };
    assert!(error.to_string().contains(TAPE), "`{error}` does not name `{TAPE}`");
}
