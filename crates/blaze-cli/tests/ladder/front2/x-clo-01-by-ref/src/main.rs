//! x-clo-01: a closure capturing by reference sees writes made after it was made.
//!
//! The Reference, "Closure expressions", "Capture modes": "Closures capture variables by the
//! first mode in this list that allows the closure to compile: immutable borrow, unique
//! immutable borrow, mutable borrow, move"; `c.get()` needs only `&c`, so `c` is borrowed, and
//! a `Cell` may be written through that shared borrow.
//! 1. f = || c.get() made when c = 1; c.set(5); f() = 5 (a copy taken at creation would give 1).
//! 2. g = |k| c.get() * k: after c.set(7), g(3) = 21.
//! 3. a closure reading a plain local by reference after the local was assigned before it was
//!    made: x = 4 then h = || x + 1: h() = 5; and called twice: 5 again (Fn).
//! A wrong builder that captures by value exits 1 (1).

use core::cell::Cell;

fn main() -> std::entry::Status {
    let c = Cell::new(1u32);
    let f = || c.get();
    c.set(5);
    let one = f();
    let g = |k: u32| c.get() * k;
    c.set(7);
    let two = g(3);
    let mut x = 1u32;
    x += 3;
    let h = || x + 1;
    let status = if one != 5 {
        1
    } else if two != 21 {
        2
    } else if h() != 5 || h() != 5 {
        3
    } else {
        0
    };
    std::entry::Status(status)
}
