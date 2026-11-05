//@ expect: reject "is not yet stable as a const fn"
//@ edition: 2021
//@ issue: #1440
// rustc rejects a call of an atomic's `load` in a `const fn` of a crate without
// `#![feature(const_atomic)]`: the pinned nightly's atomics are `rustc_const_unstable`. The error
// has no code ("`<path>` is not yet stable as a const fn"). blaze accepted it.
#![crate_type = "lib"]

use std::sync::atomic::{AtomicU32, Ordering};

pub const fn current(counter: &AtomicU32) -> u32 {
    counter.load(Ordering::SeqCst)
}
