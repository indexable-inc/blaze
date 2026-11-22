//! x-match-04: tuple patterns: wildcards in either place, bindings with a guard.
//!
//! The Reference, "Tuple patterns": "Tuple patterns match tuple values that match all criteria
//! defined by its subpatterns"; arms are tried in order ("Match expressions": "The first arm
//! with a matching pattern is chosen").
//! 1. (0, 0): arm `(0, 0)`: 1.          2. (0, 7): `(0, _)`: 2.
//! 3. (7, 0): `(_, 0)`: 3.              4. (4, 4): `(x, y) if x == y`: 4 + 4 = 8.
//! 5. (4, 5): the guard fails, `(x, y)`: 4 * 10 + 5 = 45.
//! A wrong builder that tests only the first element exits 2 (1, not 2).

fn pair(t: (u32, u32)) -> u32 {
    match t {
        (0, 0) => 1,
        (0, _) => 2,
        (_, 0) => 3,
        (x, y) if x == y => x + y,
        (x, y) => x * 10 + y,
    }
}

fn main() -> std::entry::Status {
    let status = if pair((0, 0)) != 1 {
        1
    } else if pair((0, 7)) != 2 {
        2
    } else if pair((7, 0)) != 3 {
        3
    } else if pair((4, 4)) != 8 {
        4
    } else if pair((4, 5)) != 45 {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
