//! rustc's E0521 "borrowed data escapes outside of closure" from a closure flow's outlives result
//! (closure.rs `escapes_closure`).
//!
//! The rule (rustc region_infer `try_propagate_universal_region_error`): a result `'x: 'c`
//! whose longer region `'x` is local to the closure, a region its expecting clause's `for<..>`
//! binds, is moved to the owner only through a region the owner names that `'x` is known to
//! outlive (`non_local_lower_bound`). With none, it is an error in the closure. Origins below:
//! 0 is Late (`e` of `on(|e| ..)`), 1 is Capture(1) (`keep`'s region), 2 is SigOwner(0).
//! NOT YET RUN: written by a write-only lane (2026-11-21).

use blaze_flow::Origin;

use crate::{External, escapes_closure};

const EXTERNALS: &[(Origin, External)] = &[(Origin(0), External::Late), (Origin(1), External::Capture(1)), (Origin(2), External::SigOwner(0))];

/// `keep = Some(e)`: `'x: 'keep` with nothing stated: E0521 (front2_closure_sigs.rs
/// `user_keeps`). Prints "late into capture, nothing known: true".
#[test]
fn a_late_region_into_a_capture_with_nothing_known_escapes() {
    let got = escapes_closure(EXTERNALS, &[], Origin(0), Origin(1), crate::region_fixture::alloc());
    std::println!("late into capture, nothing known: {got}");
    assert!(got);
}

/// TWIN: the clause's implied bounds state `'x: 'o` for an owner region `'o` (SigOwner(0)):
/// rustc moves the result to the owner as `'o: 'keep`. Not E0521 (the caller refuses it by
/// name: that move is not made yet). Prints "late into capture, known to outlive an owner
/// region: false".
#[test]
fn a_late_region_known_to_outlive_an_owner_region_does_not_escape() {
    let got = escapes_closure(EXTERNALS, &[(Origin(0), Origin(2))], Origin(0), Origin(1), crate::region_fixture::alloc());
    std::println!("late into capture, known to outlive an owner region: {got}");
    assert!(!got);
}

/// TWIN: the reverse direction, `'keep: 'x` (a capture's data handed to the call), is no
/// escape: the owner can be asked for it. Prints "capture into late: false".
#[test]
fn a_capture_into_a_late_region_is_no_escape() {
    let got = escapes_closure(EXTERNALS, &[], Origin(1), Origin(0), crate::region_fixture::alloc());
    std::println!("capture into late: {got}");
    assert!(!got);
}
