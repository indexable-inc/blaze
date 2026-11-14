use std::collections::{BTreeSet, VecDeque};

use super::*;
use crate::{Broke, Config, Record, Tape};

fn in_order(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let calm = Tape::calm();
    let mut flight = match plant {
        Plant::None => Flight::new(),
        plant => Flight::planted(plant),
    };
    let mut asked = VecDeque::new();
    let steps = tape.vec(0..=60, "steps", |t| t.below(3, "step"));
    let mut next = 0u32;
    for (at, step) in steps.into_iter().enumerate() {
        if step == 0 {
            let want = asked.pop_front();
            let got = flight.deliver(&calm, Nanos(u64::MAX));
            if got != want {
                return Err(Broke::Oracle(format!("step {at}: delivered {got:?}, asked first {want:?}")));
            }
        } else {
            flight.ask_at(Nanos(u64::from(next % 5)), next);
            asked.push_back(next);
            next += 1;
        }
    }
    while let Some(want) = asked.pop_front() {
        let got = flight.deliver(&calm, Nanos(u64::MAX));
        if got != Some(want) {
            return Err(Broke::Oracle(format!("at the end: delivered {got:?}, asked first {want}")));
        }
    }
    Ok(())
}

/// The invariant: on a calm tape the answers are delivered in the order they were asked.
#[test]
fn calm_delivers_in_the_order_asked() {
    crate::check(Config::cases(200), |tape| in_order(tape, Plant::None));
}

#[test]
fn the_oracle_catches_a_deliver_that_swap_removes() {
    crate::control(Config::cases(200), Plant::None, Plant::SwapRemove, "delivered", in_order);
}

#[test]
fn every_order_of_three_is_delivered_by_some_seed_below_64() {
    let mut orders = BTreeSet::new();
    for seed in 0..64 {
        let tape = Tape::seeded(seed);
        let mut flight = Flight::new();
        for answer in ['a', 'b', 'c'] {
            flight.ask(answer);
        }
        let order: String = (0..3).map(|_| flight.deliver(&tape, Nanos(0)).expect("three are due")).collect();
        assert!(flight.is_empty());
        orders.insert(order);
    }
    let all: BTreeSet<String> = ["abc", "acb", "bac", "bca", "cab", "cba"].into_iter().map(String::from).collect();
    assert_eq!(orders, all);
}

#[test]
fn nothing_is_delivered_before_it_is_due() {
    let tape = Tape::calm();
    let mut flight = Flight::new();
    flight.ask_at(Nanos(10), 'a');
    flight.ask_at(Nanos(5), 'b');
    assert_eq!(flight.earliest(), Some(Nanos(5)));
    assert_eq!(flight.deliver(&tape, Nanos(4)), None);
    let mut record = Record::new();
    tape.finish(&mut record);
    assert!(record.choices.is_empty(), "nothing due draws nothing");
    assert_eq!(flight.deliver(&tape, Nanos(5)), Some('b'));
    assert_eq!(flight.earliest(), Some(Nanos(10)));
    assert_eq!(flight.deliver(&tape, Nanos(9)), None);
    assert_eq!(flight.deliver(&tape, Nanos(10)), Some('a'));
    assert_eq!(flight.earliest(), None);
    assert_eq!(flight.len(), 0);
}

#[test]
fn a_kill_empties_it_and_says_how_many() {
    let tape = Tape::calm();
    let mut flight = Flight::new();
    flight.ask(1);
    flight.ask(2);
    flight.ask_at(Nanos(7), 3);
    assert_eq!(flight.len(), 3);
    assert_eq!(flight.kill(), 3);
    assert!(flight.is_empty());
    assert_eq!(flight.deliver(&tape, Nanos(u64::MAX)), None);
    assert_eq!(flight.kill(), 0);
}
