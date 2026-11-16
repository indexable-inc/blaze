//! `#[bstd::main]` and `#[bstd::test]` from outside bstd, as a program's crate meets them:
//! roots picked by type, and a simulation test under its default seeds and under one seed.

#![feature(macro_attr)]

use core::time::Duration;

use bstd::{
    Cx, Io,
    clock::Clock,
    entry::{Error, Exit, Status},
};

/// A `main` whose roots are a context, a world it does not use, and a clock by type.
mod program {
    use super::{Clock, Cx, Duration, Error, Io};

    #[bstd::main]
    pub fn main(cx: &Cx, _: Io, clock: Clock) -> Result<(), Error> {
        let before = clock.now();
        clock.sleep(cx, Duration::from_millis(1))?;
        match clock.now().since(before) {
            Some(_) => Ok(()),
            None => Err(Error::new("the clock went back")),
        }
    }
}

/// The process's `main` that `#[bstd::main]` made runs the function on the native world and
/// ends as it returned.
#[test]
fn main_is_handed_its_roots_by_type() {
    assert_eq!(program::main(), Exit(Status::OK));
}

/// A simulation test: every default seed, with its faults on.
#[bstd::test]
fn a_sleep_ends_after_its_wait(cx: &Cx, io: Io) -> Result<(), Error> {
    let clock = io.need_clock()?;
    let before = clock.now();
    clock.sleep(cx, Duration::from_millis(5))?;
    let slept = clock.now().since(before).ok_or_else(|| Error::new("the clock went back"))?;
    if slept < Duration::from_millis(5) {
        return Err(Error::new("the sleep ended before its wait"));
    }
    Ok(())
}

/// The same test on one seed, given by the attribute.
#[bstd::test(seed = 7)]
fn one_seed_is_given_by_the_attribute(cx: &Cx, io: Io) -> Result<(), Error> {
    let clock = io.need_clock()?;
    clock.sleep(cx, Duration::from_millis(1))?;
    Ok(())
}
