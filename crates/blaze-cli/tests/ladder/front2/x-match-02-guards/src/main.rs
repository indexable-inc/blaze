//! x-match-02: a guard that fails falls through to the next pattern, even inside one `|` arm.
//!
//! The Reference, "Match guards": "When the pattern matches successfully, the pattern guard
//! expression is executed. If the expression evaluates to true, the pattern is successfully
//! matched against. Otherwise, the next pattern, including other matches with the `|`
//! operator in the same arm, is tested." Its own example: `match 1 { 1 | _ if { i.set(i.get()
//! + 1); false } => {} _ => {} }` leaves `i` at 2: the guard runs once per alternative.
//! 1. pick(7): `n if n > 10` fails, `n if n > 5` passes: 2 (the arm `7 => 3` is never reached).
//! 2. pick(3): both guards fail, `7` does not match: `_` gives 4.
//! 3. pick(12): the first guard passes: 1.
//! 4. the Reference's example: the guard ran twice: 2.
//! A wrong builder that commits to an arm once its pattern matches exits 1 or 2 (no arm left).

use core::cell::Cell;

fn pick(x: u32) -> u32 {
    match x {
        n if n > 10 => 1,
        n if n > 5 => 2,
        7 => 3,
        _ => 4,
    }
}

fn main() -> std::entry::Status {
    let i = Cell::new(0);
    match 1 {
        1 | _ if {
            i.set(i.get() + 1);
            false
        } => {}
        _ => {}
    }
    let status = if pick(7) != 2 {
        1
    } else if pick(3) != 4 {
        2
    } else if pick(12) != 1 {
        3
    } else if i.get() != 2 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
