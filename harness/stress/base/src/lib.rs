//! The bottom crate of the stress workspace: shapes, a generic container, macros, statics.
pub mod shapes;
pub mod stack;

use std::sync::atomic::{AtomicU32, Ordering};

pub const SCALE: u32 = 3;
pub const TABLE: [u8; 4] = [1, 2, 3, 4];
pub static CALLS: AtomicU32 = AtomicU32::new(0);
pub static GREETING: &str = "hello";

pub const fn scaled(x: u32) -> u32 {
    x * SCALE
}

pub fn count() -> u32 {
    CALLS.fetch_add(1, Ordering::Relaxed) + 1
}

#[macro_export]
macro_rules! square {
    ($x:expr) => {
        $x * $x
    };
}

#[macro_export]
macro_rules! make_adder {
    ($name:ident, $n:expr) => {
        pub fn $name(x: u32) -> u32 {
            x + $n
        }
    };
}

make_adder!(add_two, 2);

pub fn digest(bytes: &[u8]) -> u32 {
    let mut h = 17u32;
    for &b in bytes {
        h = h.wrapping_mul(31).wrapping_add(b as u32);
    }
    h
}

pub fn largest<T: PartialOrd + Copy>(items: &[T]) -> T {
    let mut best = items[0];
    for &item in items {
        if item > best {
            best = item;
        }
    }
    best
}

pub fn where_am_i() -> u32 {
    line!()
}

#[track_caller]
pub fn caller_line() -> u32 {
    std::panic::Location::caller().line()
}
