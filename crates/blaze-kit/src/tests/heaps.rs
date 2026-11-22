//! `Heap4` against a sorted `Vec`, `TimerWheel` against a list of deadlines.

use std::{format, vec::Vec as StdVec};

use blaze_sim::{Broke, Config, Tape};

use super::{Failing, Why, counted, ensure, no_leaks, planted, step, under_every_failure};
use crate::{Heap4, TimerWheel, plant::Plant, wheel::Timer};

fn heap_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = counted();
    {
        let mut heap: Heap4<u64, _> = Heap4::new_in(&alloc);
        let mut model: StdVec<u64> = StdVec::new();
        for _ in 0..tape.below(200, "ops") {
            let value = tape.below(500, "value");
            match tape.below(5, "op") {
                0..=1 => {
                    heap.push(value);
                    model.push(value);
                }
                2..=3 => {
                    model.sort_unstable_by(|a, b| b.cmp(a));
                    ensure(heap.pop() == model.pop(), || format!("pop gave other than the least"))?;
                }
                _ => {
                    model.push(value);
                    model.sort_unstable_by(|a, b| b.cmp(a));
                    let least = model.pop();
                    ensure(Some(heap.push_pop(value)) == least, || format!("push_pop gave other than the least"))?;
                }
            }
            ensure(heap.len() == model.len(), || format!("len differs from the model"))?;
            ensure(heap.peek().copied() == model.iter().copied().min(), || format!("peek gave other than the least"))?;
        }
    }
    no_leaks(&alloc)
}

/// The refinement: `Heap4` matches a sorted `Vec` and `TimerWheel` matches a list of deadlines under any sequence of operations.
#[test]
fn heap4_matches_a_sorted_vec() {
    blaze_sim::check(Config::cases(300), heap_case);
}

#[test]
fn the_oracle_catches_a_sift_that_skips_the_fourth_child() {
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::HeapSkipsFourthChild, "other than the least", |tape, p| {
        planted(p, tape, heap_case)
    });
}

/// A span of ticks of any scale: below `2^k` for a drawn `k`.
fn ticks(tape: &Tape, label: &'static str) -> u64 {
    let k = tape.below(44, "scale");
    tape.below(1u64 << k, label)
}

fn wheel_case(tape: &Tape) -> Result<(), Broke> {
    let alloc = counted();
    {
        let start = tape.next_u64("start") >> tape.below(64, "start scale");
        let start = start.min(u64::MAX >> 2);
        let mut now = start;
        let mut wheel: TimerWheel<u32, _> = TimerWheel::new_in(now, &alloc);
        // Each timer made: its handle, deadline, and whether it waits.
        let mut model: StdVec<(Timer, u64, bool)> = StdVec::new();
        for _ in 0..tape.below(120, "ops") {
            match tape.below(6, "op") {
                0..=2 => {
                    let deadline = now + ticks(tape, "in");
                    let tag = blaze_int::Int::truncate_to::<u32>(model.len());
                    let timer = wheel.insert(deadline, tag);
                    model.push((timer, deadline, true));
                }
                3 if !model.is_empty() => {
                    let k = tape.index(model.len(), "which");
                    let got = wheel.cancel(model[k].0);
                    let want = model[k].2.then(|| blaze_int::Int::truncate_to::<u32>(k));
                    ensure(got == want, || format!("cancel of timer {k} gave {got:?}, the model {want:?}"))?;
                    model[k].2 = false;
                }
                _ => {
                    now += ticks(tape, "advance");
                    let lower = wheel.next_deadline();
                    let least = model.iter().filter(|t| t.2).map(|t| t.1).min();
                    ensure(lower.is_some() == least.is_some(), || format!("next_deadline {lower:?} with the least {least:?}"))?;
                    if let (Some(lower), Some(least)) = (lower, least) {
                        ensure(lower <= least.max(wheel.elapsed()), || format!("next_deadline {lower} is past the least {least}"))?;
                    }
                    while let Some((timer, tag)) = wheel.poll(now) {
                        let k = blaze_int::idx(tag);
                        let Some(entry) = model.get_mut(k) else {
                            return Err(Broke::Oracle(format!("poll gave an unknown tag {tag}")));
                        };
                        ensure(entry.0 == timer && entry.2, || format!("poll gave timer {k}, which does not wait"))?;
                        ensure(entry.1 <= now, || format!("poll at {now} fired timer {k} due at {}", entry.1))?;
                        entry.2 = false;
                    }
                    if let Some(late) = model.iter().position(|t| t.2 && t.1 <= now) {
                        return Err(Broke::Oracle(format!("timer {late} due at {} still waits at {now}", model[late].1)));
                    }
                }
            }
            let waiting = model.iter().filter(|t| t.2).count();
            ensure(wheel.len() == waiting, || format!("len {} with {waiting} waiting", wheel.len()))?;
            for (k, t) in model.iter().enumerate() {
                ensure(wheel.contains(t.0) == t.2, || format!("contains of timer {k} said wrong"))?;
            }
        }
    }
    no_leaks(&alloc)
}

#[test]
fn timer_wheel_matches_a_list_of_deadlines() {
    blaze_sim::check(Config::cases(400), wheel_case);
}

#[test]
fn the_oracle_catches_a_cascade_that_skips_slots() {
    blaze_sim::control(Config::cases(800), Plant::Clean, Plant::WheelCascadesLate, "still waits", |tape, p| {
        planted(p, tape, wheel_case)
    });
}

#[test]
fn heap_and_wheel_try_forms_fail_cleanly_at_every_allocation() -> Result<(), Broke> {
    under_every_failure("Heap4", |alloc| {
        let mut heap: Heap4<u64, _> = Heap4::try_with_capacity_in(1, alloc)?;
        let look = |h: &Heap4<u64, &Failing>| {
            let mut all = StdVec::from(h.as_slice());
            all.sort_unstable();
            all
        };
        for n in [5u64, 3, 9, 1, 7, 2, 8] {
            step(&mut heap, look, |h| {
                h.try_push(n).map_err(|r| if r.value == n { Why::from(r) } else { Why::Changed(format!("{} came back", r.value)) })
            })?;
        }
        step(&mut heap, look, |h| h.try_reserve(30))?;
        Ok(())
    })?;
    under_every_failure("TimerWheel", |alloc| {
        let mut wheel: TimerWheel<u64, _> = TimerWheel::new_in(0, alloc);
        let look = |w: &TimerWheel<u64, &Failing>| (w.len(), w.next_deadline());
        for n in 0..6u64 {
            step(&mut wheel, look, |w| {
                w.try_insert(n * 1000, n).map_err(|r| if r.value == n { Why::from(r) } else { Why::Changed(format!("{} came back", r.value)) })
            })?;
        }
        step(&mut wheel, look, |w| w.try_reserve(20))?;
        Ok(())
    })?;
    Ok(())
}
