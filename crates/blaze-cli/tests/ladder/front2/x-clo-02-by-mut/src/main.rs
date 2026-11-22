//! x-clo-02: a closure capturing by mutable reference, called twice, writes the captured local.
//!
//! The Reference, "Closure expressions", "Capture modes": a closure that assigns to a
//! captured variable captures it by "mutable borrow"; each call writes the caller's variable.
//! 1. n = 0; inc = || n += 3; inc(); inc(); then n = 6.
//! 2. a closure pushing digits into a local: d = 0; push(4); push(2): d = 42.
//! 3. a closure that modifies a struct's field through its capture: p.x = 1, bump twice by 10:
//!    21; p.y untouched 9.
//! A wrong builder that captures a copy exits 1 (n = 0).

struct P {
    x: u32,
    y: u32,
}

fn main() -> std::entry::Status {
    let mut n = 0u32;
    let mut inc = || n += 3;
    inc();
    inc();
    let mut d = 0u32;
    let mut push = |k: u32| d = d * 10 + k;
    push(4);
    push(2);
    let mut p = P { x: 1, y: 9 };
    let mut bump = || p.x += 10;
    bump();
    bump();
    let status = if n != 6 {
        1
    } else if d != 42 {
        2
    } else if p.x != 21 || p.y != 9 {
        3
    } else {
        0
    };
    std::entry::Status(status)
}
