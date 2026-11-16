//! x-gen-01: one generic function instantiated at three types gives three right answers.
//!
//! The Reference, "Generic parameters" / "Functions": a generic function is checked once and
//! each use at a type runs that type's operations (`PartialOrd` of u8, i64 and f32).
//! `largest(a, b, c)` returns the greatest by `>`:
//! 1. u8: (3, 250, 7): 250 (an i8 comparison would see 250 as -6 and pick 7).
//! 2. i64: (-5, -1, -9): -1 (an unsigned comparison would pick -1 too, so also (-5, 3, -9): 3).
//! 3. f32: (1.5, -2.0, 1.25): 1.5.
//! 4. the same function at a struct with its own `PartialOrd` by one field: Score(4), Score(9),
//!    Score(2): Score(9).
//! A wrong builder that shares one instance between types exits 1, 2 or 3.

#[derive(Clone, Copy, PartialEq, PartialOrd)]
struct Score(u32);

fn largest<T: PartialOrd + Copy>(a: T, b: T, c: T) -> T {
    let mut m = a;
    if b > m {
        m = b;
    }
    if c > m {
        m = c;
    }
    m
}

fn main() -> std::entry::Status {
    let status = if largest(3u8, 250, 7) != 250 {
        1
    } else if largest(-5i64, -1, -9) != -1 || largest(-5i64, 3, -9) != 3 {
        2
    } else if largest(1.5f32, -2.0, 1.25) != 1.5 {
        3
    } else if largest(Score(4), Score(9), Score(2)).0 != 9 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
