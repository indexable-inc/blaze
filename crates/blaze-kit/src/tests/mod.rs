//! The crate's tests: per structure, a model test on `blaze_sim::check` against the obvious slow
//! structure, a planted bug its oracle catches (`blaze_sim::control`), and every `try_` method
//! run under `every_failure`, refused at each allocation in turn, which must give the refusal
//! back and leave the value as it was, leaking nothing.

use std::{alloc::System, format, string::String};

use blaze_int::Int;
use blaze_own::{
    AllocFailure, Refused,
    testing::{Counting, FailAt, every_failure},
};
use blaze_sim::{Broke, Tape};

use crate::plant::{self, Plant};

mod codec;
mod heaps;
mod inline_vec;
mod search;
mod sets;
mod table;
mod union_find;

/// The allocator of an `every_failure` run.
type Failing = FailAt<Counting<&'static System>>;

/// What a failing run came back with: a refusal (what is wanted), or a value a refused call
/// changed.
#[derive(Debug)]
enum Why {
    Refused(AllocFailure),
    Changed(String),
}

impl From<AllocFailure> for Why {
    fn from(failure: AllocFailure) -> Why {
        Why::Refused(failure)
    }
}

impl<T> From<Refused<T>> for Why {
    fn from(refused: Refused<T>) -> Why {
        Why::Refused(refused.why)
    }
}

/// One `try_` call on `value`: on a refusal, `look` must read the same before and after.
fn step<V, S: PartialEq + core::fmt::Debug, R, E: Into<Why>>(
    value: &mut V,
    look: impl Fn(&V) -> S,
    call: impl FnOnce(&mut V) -> Result<R, E>,
) -> Result<R, Why> {
    let before = look(value);
    match call(value) {
        Ok(done) => Ok(done),
        Err(refused) => {
            let after = look(value);
            if after != before {
                return Err(Why::Changed(format!("{before:?} became {after:?}")));
            }
            Err(refused.into())
        }
    }
}

/// Runs `run` refused at each allocation in turn (`every_failure`): each must come back
/// refused, unchanged, with nothing leaked.
fn under_every_failure(what: &str, mut run: impl FnMut(&Failing) -> Result<(), Why>) -> Result<usize, Broke> {
    let mut changed = None;
    let found = every_failure(&System, |alloc| match run(alloc) {
        Ok(()) => Ok(()),
        Err(Why::Refused(failure)) => Err(failure),
        Err(Why::Changed(how)) => {
            if changed.is_none() {
                changed = Some(how);
            }
            Ok(())
        }
    });
    if let Some(how) = changed {
        return Err(Broke::Oracle(format!("{what}: a refused call changed its value: {how}")));
    }
    let events = found.map_err(|e| Broke::Oracle(format!("{what}: {e}")))?;
    if events == 0 {
        return Err(Broke::Harness(format!("{what}: the run allocated nothing, so nothing was refused")));
    }
    Ok(events)
}

/// The oracle's check: `holds`, or a finding saying `why`.
fn ensure(holds: bool, why: impl FnOnce() -> String) -> Result<(), Broke> {
    if holds { Ok(()) } else { Err(Broke::Oracle(why())) }
}

/// A draw below `n`, as a `u32`.
fn small(tape: &Tape, n: u32, label: &'static str) -> u32 {
    // Below `n`, a `u32`: the low bits are the value.
    tape.below(u64::from(n), label).truncate_to::<u32>()
}

/// The allocator a model test runs in, checked for leaks after.
fn counted() -> Counting<&'static System> {
    Counting::new(&System)
}

/// The leak check at the end of a model case.
fn no_leaks(alloc: &Counting<&'static System>) -> Result<(), Broke> {
    alloc.check().map_err(|leaked| Broke::Oracle(format!("leaked: {leaked}")))
}

/// Runs a model case with `plant` planted, for a control.
fn planted(plant: Plant, tape: &Tape, case: fn(&Tape) -> Result<(), Broke>) -> Result<(), Broke> {
    plant::with(plant, || case(tape))
}
