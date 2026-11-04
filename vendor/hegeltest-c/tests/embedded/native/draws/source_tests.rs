//! Tests for `src/native/draws/source.rs`: that a test case answers the
//! `Draws` operations as its own methods do, that drawing from an RNG
//! alone samples exactly as a fresh test case does, and that the
//! simplest source answers every choice's simplest value.

use super::*;
use crate::native::core::GenerationParameters;
use crate::native::draws::many_more;
use crate::native::rng::EngineRng;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;
use rand::TryRng;

fn letters() -> Arc<IntervalSet> {
    Arc::new(IntervalSet::new(vec![(u32::from('a'), u32::from('z'))]).unwrap())
}

/// Every operation of the trait, drawn once each from `d`, as values
/// that compare between sources.
fn exercise<D: Draws>(d: &mut D, state: &mut ManyState) -> Vec<String> {
    vec![
        d.draw_integer_uniform(0, 10).unwrap().to_string(),
        d.draw_integer_uniform(5, u64::MAX).unwrap().to_string(),
        d.draw_integer_i64(0, 1).unwrap().to_string(),
        d.draw_integer_i64(-100, 100).unwrap().to_string(),
        d.draw_integer_i64(i64::MIN, i64::MAX).unwrap().to_string(),
        d.weighted(0.3, None).unwrap().to_string(),
        d.weighted_against(0.3, 100, None).unwrap().to_string(),
        d.weighted_precise(0.3, None).unwrap().to_string(),
        d.many_more(state).unwrap().to_string(),
        d.draw_string(letters(), 0, 4).unwrap(),
    ]
}

#[test]
fn a_test_case_draws_through_the_trait_as_through_its_own_methods() {
    let mut a = NativeTestCase::new_random(EngineRng::seeded(7)).unwrap();
    let mut b = NativeTestCase::new_random(EngineRng::seeded(7)).unwrap();
    let mut state_a = ManyState::new(1, Some(5));
    let mut state_b = ManyState::new(1, Some(5));
    for _ in 0..50 {
        assert_eq!(
            Draws::draw_integer_uniform(&mut a, 3, 1000).unwrap(),
            b.draw_integer_uniform(3, 1000).unwrap()
        );
        assert_eq!(
            Draws::draw_integer_i64(&mut a, -20, 20).unwrap(),
            b.draw_integer_i64(-20, 20).unwrap()
        );
        assert_eq!(
            Draws::weighted(&mut a, 0.3, None).unwrap(),
            b.weighted(0.3, None).unwrap()
        );
        assert_eq!(
            Draws::weighted_against(&mut a, 0.3, 100, None).unwrap(),
            b.weighted_against(0.3, 100, None).unwrap()
        );
        assert_eq!(
            Draws::weighted_precise(&mut a, 0.3, None).unwrap(),
            b.weighted_precise(0.3, None).unwrap()
        );
        assert_eq!(
            a.many_more(&mut state_a).unwrap(),
            many_more(&mut b, &mut state_b).unwrap()
        );
    }
    assert_eq!(state_a.count, state_b.count);
    let depth = a.span_depth();
    Draws::start_span(&mut a, 11);
    assert_eq!(a.span_depth(), depth + 1);
    Draws::stop_span(&mut a, true);
    assert_eq!(a.span_depth(), depth);
    assert!(a.has_discards);
    Draws::conclude(&mut a, Status::Invalid);
    assert_eq!(a.status(), Some(Status::Invalid));
    Draws::conclude(&mut a, Status::Valid);
    assert_eq!(a.status(), Some(Status::Invalid));
}

#[test]
fn rejecting_below_the_minimum_concludes_the_test_case_invalid() {
    let mut ntc = NativeTestCase::new_random(EngineRng::seeded(0)).unwrap();
    let mut state = ManyState::new(2, Some(5));
    for _ in 0..3 {
        assert!(ntc.many_more(&mut state).unwrap());
        ntc.many_reject(&mut state).unwrap();
    }
    assert!(ntc.many_more(&mut state).unwrap());
    assert!(matches!(
        Draws::many_reject(&mut ntc, &mut state).unwrap_err(),
        EngineError::InvalidTestCase
    ));
    assert_eq!(ntc.status(), Some(Status::Invalid));
}

#[test]
fn rejecting_past_the_minimum_stops_the_collection() {
    let mut ntc = NativeTestCase::new_random(EngineRng::seeded(0)).unwrap();
    let mut state = ManyState::new(0, Some(50));
    let mut drawn = 0;
    while ntc.many_more(&mut state).unwrap() {
        drawn += 1;
        ntc.many_reject(&mut state).unwrap();
        assert!(drawn < 100);
    }
    assert!(state.force_stop);
    assert_eq!(state.count, 0);
}

#[test]
fn drawing_from_an_rng_samples_as_a_fresh_test_case_does() {
    for seed in 0..8 {
        let mut ntc = NativeTestCase::new_random(EngineRng::seeded(seed)).unwrap();
        let mut rng = EngineRng::seeded(seed);
        let params = GenerationParameters::draw(&mut rng).unwrap();
        let mut draws = RngDraws::new(&mut rng, params.integer);
        let mut state_a = ManyState::new(0, Some(10));
        let mut state_b = ManyState::new(0, Some(10));
        for _ in 0..20 {
            assert_eq!(
                exercise(&mut ntc, &mut state_a),
                exercise(&mut draws, &mut state_b),
                "seed {seed}"
            );
        }
        assert_eq!(draws.status(), None);
    }
}

#[test]
fn forced_booleans_consume_no_entropy_from_an_rng() {
    let mut rng = EngineRng::seeded(5);
    let mut draws = RngDraws::new(&mut rng, GenerationParameters::default().integer);
    assert!(!draws.weighted(0.0, None).unwrap());
    assert!(draws.weighted(1.0, None).unwrap());
    assert!(draws.weighted(0.5, Some(true)).unwrap());
    assert!(!draws.weighted_against(0.0, 1, None).unwrap());
    assert!(draws.weighted_against(0.5, 128, Some(true)).unwrap());
    assert!(draws.weighted_precise(1.0, None).unwrap());
    assert!(!draws.weighted_precise(0.5, Some(false)).unwrap());
    assert_eq!(
        rng.try_next_u64().unwrap(),
        EngineRng::seeded(5).try_next_u64().unwrap()
    );
}

#[test]
fn an_rng_source_keeps_its_first_conclusion_and_ignores_spans() {
    let mut rng = EngineRng::seeded(0);
    let mut draws = RngDraws::new(&mut rng, GenerationParameters::default().integer);
    draws.start_span(7);
    draws.stop_span(true);
    assert_eq!(draws.status(), None);
    draws.conclude(Status::Invalid);
    assert_eq!(draws.status(), Some(Status::Invalid));
    draws.conclude(Status::Valid);
    assert_eq!(draws.status(), Some(Status::Invalid));
}

#[test]
fn the_simplest_source_answers_every_choice_at_its_simplest() {
    let mut draws = SimplestDraws::default();
    assert_eq!(draws.draw_integer_uniform(0, 10).unwrap(), 0);
    assert_eq!(draws.draw_integer_uniform(3, 10).unwrap(), 3);
    assert_eq!(draws.draw_integer_uniform(7, u64::MAX).unwrap(), 7);
    assert_eq!(draws.draw_integer_i64(-5, 5).unwrap(), 0);
    assert_eq!(draws.draw_integer_i64(2, 9).unwrap(), 2);
    assert_eq!(draws.draw_integer_i64(-9, -2).unwrap(), -2);
    assert!(!draws.weighted(0.5, None).unwrap());
    assert!(draws.weighted(0.5, Some(true)).unwrap());
    assert!(draws.weighted(1.0, None).unwrap());
    assert!(!draws.weighted_against(0.99, 1, None).unwrap());
    assert!(draws.weighted_against(0.5, 128, Some(true)).unwrap());
    assert!(!draws.weighted_precise(0.999, None).unwrap());
    assert!(draws.weighted_precise(1.0, None).unwrap());
    let mut state = ManyState::new(2, Some(5));
    assert!(draws.many_more(&mut state).unwrap());
    assert!(draws.many_more(&mut state).unwrap());
    assert!(!draws.many_more(&mut state).unwrap());
    assert_eq!(state.count, 2);
    assert_eq!(draws.draw_string(letters(), 3, 8).unwrap(), "aaa");
    assert_eq!(draws.draw_string(letters(), 0, 8).unwrap(), "");
    assert_eq!(draws.status(), None);
    assert!(draws.simplest());
}

/// Only the simplest source says it is, so only it has a walk take the
/// least of an alternation's strings.
#[test]
fn only_the_simplest_source_answers_that_it_is() {
    let mut rng = EngineRng::seeded(0);
    assert!(!RngDraws::new(&mut rng, GenerationParameters::default().integer).simplest());
    assert!(
        !NativeTestCase::new_random(EngineRng::seeded(0))
            .unwrap()
            .simplest()
    );
    assert!(SimplestDraws::default().simplest());
}

#[test]
fn the_simplest_source_keeps_its_first_conclusion_and_ignores_spans() {
    let mut draws = SimplestDraws::default();
    draws.start_span(7);
    draws.stop_span(false);
    draws.conclude(Status::Valid);
    draws.conclude(Status::Invalid);
    assert_eq!(draws.status(), Some(Status::Valid));
}
