//! x-match-08: or-patterns at the top, nested inside a variant, and binding in each alternative.
//!
//! The Reference, "Or-patterns": "Or-patterns are patterns that match on one of two or more
//! sub-patterns (for example `A | B | C`)"; "every binding must be bound in each sub-pattern".
//! 1. 2 is in `1 | 2 | 3`: 1.      2. 5 is in `4 | 5`: 2.      3. 6: `_`, 3.
//! 4. Some(2) against `Some(1 | 2)`: 10; Some(3): `Some(_)`, 20; None: 30.
//! 5. (8, 0) against `(x, 0) | (0, x)`: x = 8; (0, 6): x = 6 (the second alternative binds the
//!    second element); (3, 4): 0.
//! A wrong builder that tests only the first alternative exits 1 (2 is not 1).

fn small(x: u32) -> u32 {
    match x {
        1 | 2 | 3 => 1,
        4 | 5 => 2,
        _ => 3,
    }
}

fn inner(o: Option<u32>) -> u32 {
    match o {
        Some(1 | 2) => 10,
        Some(_) => 20,
        None => 30,
    }
}

fn either(t: (u32, u32)) -> u32 {
    match t {
        (x, 0) | (0, x) => x,
        _ => 0,
    }
}

fn main() -> std::entry::Status {
    let status = if small(2) != 1 {
        1
    } else if small(5) != 2 {
        2
    } else if small(6) != 3 {
        3
    } else if inner(Some(2)) != 10 || inner(Some(3)) != 20 || inner(None) != 30 {
        4
    } else if either((8, 0)) != 8 || either((0, 6)) != 6 || either((3, 4)) != 0 {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
