//! `InlineVec` against `Vec`.

use std::{format, vec::Vec as StdVec};

use blaze_sim::{Broke, Config, Tape};

use super::{Failing, Why, counted, ensure, no_leaks, planted, step, under_every_failure};
use crate::{InlineVec, plant::Plant};

fn case(tape: &Tape) -> Result<(), Broke> {
    let alloc = counted();
    {
        let mut v: InlineVec<u64, 4, _> = InlineVec::new_in(&alloc);
        let mut model: StdVec<u64> = StdVec::new();
        for _ in 0..tape.below(60, "ops") {
            let value = tape.below(1000, "value");
            let what = match tape.below(12, "op") {
                0..=3 => {
                    v.push(value);
                    model.push(value);
                    "push"
                }
                4 => {
                    ensure(v.pop() == model.pop(), || format!("pop differs from the model"))?;
                    "pop"
                }
                5 => {
                    let at = tape.index(model.len() + 1, "at");
                    v.insert(at, value);
                    model.insert(at, value);
                    "insert"
                }
                6 if !model.is_empty() => {
                    let at = tape.index(model.len(), "at");
                    ensure(v.remove(at) == model.remove(at), || format!("remove differs from the model"))?;
                    "remove"
                }
                7 if !model.is_empty() => {
                    let at = tape.index(model.len(), "at");
                    ensure(v.swap_remove(at) == model.swap_remove(at), || format!("swap_remove differs"))?;
                    "swap_remove"
                }
                8 => {
                    let len = tape.index(model.len() + 1, "len");
                    v.truncate(len);
                    model.truncate(len);
                    "truncate"
                }
                9 => {
                    let start = tape.index(model.len() + 1, "start");
                    let end = start + tape.index(model.len() - start + 1, "end");
                    v.remove_range(start..end);
                    model.drain(start..end);
                    "remove_range"
                }
                10 => {
                    let keep = tape.below(3, "keep") + 2;
                    v.retain(|x| *x % keep != 0);
                    model.retain(|x| *x % keep != 0);
                    "retain"
                }
                _ => {
                    let more = [value, value + 1, value + 2];
                    v.try_extend_from_slice(&more).map_err(|e| Broke::Harness(format!("{e}")))?;
                    model.extend_from_slice(&more);
                    if tape.coin("shrink") {
                        v.try_shrink_to_fit().map_err(|e| Broke::Harness(format!("{e}")))?;
                        ensure(v.spilled() == (model.len() > 4), || format!("shrink_to_fit left it spilled at {}", model.len()))?;
                    }
                    "extend"
                }
            };
            ensure(v.as_slice() == model.as_slice(), || format!("after {what} the vector differs from the model: {v:?} vs {model:?}"))?;
            ensure(v.capacity() >= v.len(), || format!("capacity below length after {what}"))?;
            if tape.one_in(8, "clone") {
                let copy = v.clone();
                ensure(copy == v, || format!("a clone differs"))?;
            }
        }
    }
    no_leaks(&alloc)
}

/// The refinement: `InlineVec` matches `Vec` under any sequence of operations including spilling from inline to heap storage.
#[test]
fn inline_vec_matches_a_vec() {
    blaze_sim::check(Config::cases(400), case);
}

#[test]
fn the_oracle_catches_a_spill_that_drops_the_last() {
    blaze_sim::control(Config::cases(400), Plant::Clean, Plant::SpillDropsLast, "differs from the model", |tape, p| {
        planted(p, tape, case)
    });
}

#[test]
fn inline_vec_try_forms_fail_cleanly_at_every_allocation() -> Result<(), Broke> {
    under_every_failure("InlineVec", |alloc| {
        let look = |v: &InlineVec<u64, 2, &Failing>| StdVec::from(v.as_slice());
        let mut v: InlineVec<u64, 2, _> = InlineVec::try_with_capacity_in(1, alloc)?;
        for n in 0..9u64 {
            step(&mut v, look, |v| {
                v.try_push(n).map_err(|r| if r.value == n { Why::from(r) } else { Why::Changed(format!("{} came back", r.value)) })
            })?;
        }
        step(&mut v, look, |v| v.try_insert(0, 100))?;
        step(&mut v, look, |v| v.try_extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]))?;
        step(&mut v, look, |v| v.try_reserve(50))?;
        step(&mut v, look, |v| v.try_reserve_exact(100))?;
        v.truncate(5);
        step(&mut v, look, InlineVec::try_shrink_to_fit)?;
        let copy = step(&mut v, look, |v| v.try_clone())?;
        if copy != v {
            return Err(Why::Changed(format!("a clone differs")));
        }
        let from = InlineVec::<u64, 2, _>::try_from_slice_in(&[1, 2, 3], alloc)?;
        drop(from);
        Ok(())
    })?;
    Ok(())
}
