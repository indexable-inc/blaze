//! `#[bstd::main]` and `#[bstd::test]` from outside bstd, as a program's crate meets them:
//! grants picked by type, for a `main` and for a simulation test (D104, D109), the test under
//! its default seeds and under one seed.

#![feature(macro_attr)]

use core::time::Duration;

use bstd::{
    Cx,
    clock::Clock,
    entry::{Error, Exit, Status},
};

/// A `main` whose roots are a context and the one grant it uses, a clock (D104: a whole `Io`
/// cannot be asked for).
mod program {
    use super::{Clock, Cx, Duration, Error};

    #[bstd::main]
    pub fn main(cx: &Cx, clock: Clock) -> Result<(), Error> {
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

/// A simulation test that takes a clock: every default seed, with the one fault kind a clock
/// can meet (late completions) on.
#[bstd::test]
fn a_sleep_ends_after_its_wait(cx: &Cx, clock: Clock) -> Result<(), Error> {
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
fn one_seed_is_given_by_the_attribute(cx: &Cx, clock: Clock) -> Result<(), Error> {
    clock.sleep(cx, Duration::from_millis(1))?;
    Ok(())
}
