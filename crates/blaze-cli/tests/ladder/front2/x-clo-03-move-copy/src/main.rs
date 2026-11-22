//! x-clo-03: a `move` closure COPIES a Copy value: the original stays readable and unchanged.
//!
//! The Reference, "Closure expressions": "If the `move` keyword is used, then all captures are
//! by move or, for Copy types, by copy"; the closure's copy is its own state.
//! 1. x = 5; f = move || x + 1: f() = 6; x still 5.
//! 2. y = 1; g = move || { y += 10; y }: g() = 11, g() = 21 (its own copy accumulates);
//!    y still 1.
//! 3. after the closure is made, writing the original (x = 50) does not change f: f() = 6.
//! A wrong builder that captures by reference exits 2 (y = 21) or 3 (f() = 51).

fn main() -> std::entry::Status {
    let mut x = 5u32;
    let f = move || x + 1;
    let one = f() == 6 && x == 5;
    let mut y = 1u32;
    let mut g = move || {
        y += 10;
        y
    };
    let g1 = g();
    let g2 = g();
    x = 50;
    let status = if !one {
        1
    } else if g1 != 11 || g2 != 21 || y != 1 {
        2
    } else if f() != 6 || x != 50 {
        3
    } else {
        0
    };
    std::entry::Status(status)
}
