//! x-name-02: a unit variant brought in by `use` is matched by its bare name.
//!
//! The Reference, "Identifier patterns": "Path patterns take precedence over identifier
//! patterns." After `use Color::{Blue, Green, Red};` each bare name resolves to its unit
//! variant, so `Red` in a pattern matches only `Red`.
//! 1. code(Blue): arms `Red`, `Green`, `Blue`: 3.      2. code(Green): 2.      3. code(Red): 1.
//! 4. A unit struct `Unit` in a `let` pattern and a `match`: matches, 9.
//! A WRONG builder that reads `Red` as a binding takes the first arm for every color: exits 1
//! (code(Blue) = 1).

use Color::{Blue, Green, Red};

#[derive(Clone, Copy)]
enum Color {
    Red,
    Green,
    Blue,
}

struct Unit;

fn code(c: Color) -> u32 {
    match c {
        Red => 1,
        Green => 2,
        Blue => 3,
    }
}

fn unit(u: Unit) -> u32 {
    match u {
        Unit => 9,
    }
}

fn main() -> std::entry::Status {
    let status = if code(Blue) != 3 {
        1
    } else if code(Green) != 2 {
        2
    } else if code(Red) != 1 {
        3
    } else if unit(Unit) != 9 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
