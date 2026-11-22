//! x-match-09: `@` bindings: the whole value bound while a subpattern tests it.
//!
//! The Reference, "Identifier patterns": "the `@` symbol ... binds the value to the identifier
//! when the subpattern also matches", e.g. `e @ 1 ..= 5`.
//! 1. 7 against `n @ 1..=9`: n = 7, n * 10 = 70.        2. 42: `n @ 10..=99` gives n + 1 = 43.
//! 3. 0: neither range: `_`, 0.
//! 4. Some(3) against `whole @ Some(3)`: whole is Some(3): 3 + 100 = 103.
//! 5. (2, 5) against `p @ (a, b)` (Copy fields): p.0 + p.1 + a * b = 2 + 5 + 10 = 17.
//! A wrong builder that binds the subpattern's constant instead of the value exits 1 (10 or 90).

fn dec(x: u32) -> u32 {
    match x {
        n @ 1..=9 => n * 10,
        n @ 10..=99 => n + 1,
        _ => 0,
    }
}

fn main() -> std::entry::Status {
    let some = match Some(3u32) {
        whole @ Some(3) => match whole {
            Some(v) => v + 100,
            None => 1,
        },
        _ => 2,
    };
    let at_pair = match (2u32, 5u32) {
        p @ (a, b) => p.0 + p.1 + a * b,
    };
    let status = if dec(7) != 70 {
        1
    } else if dec(42) != 43 {
        2
    } else if dec(0) != 0 {
        3
    } else if some != 103 {
        4
    } else if at_pair != 17 {
        5
    } else {
        0
    };
    std::entry::Status(status)
}
