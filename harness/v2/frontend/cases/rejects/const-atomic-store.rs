//@ expect: reject "is not yet stable as a const fn"
//@ edition: 2021
//@ issue: #1440
// The kind of #1440's second case with another atomic operation: rustc rejects `AtomicBool::store`
// in a `const fn` without `#![feature(const_atomic)]`, with no error code.
#![crate_type = "lib"]

use std::sync::atomic::{AtomicBool, Ordering};

pub const fn raise(flag: &AtomicBool) {
    flag.store(true, Ordering::Release)
}
