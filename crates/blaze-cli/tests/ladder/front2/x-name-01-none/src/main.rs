//! x-name-01: a bare `None` in a pattern is the variant `Option::None`, not a new binding.
//!
//! The Reference, "Identifier patterns": "Path patterns take precedence over identifier
//! patterns." `None` resolves (through the prelude) to the unit variant `Option::None`, so the
//! pattern `None` is a path pattern that matches only `None`.
//! 1. kind(Some(5)): `None` does not match Some(5); the `Some(n)` arm gives n + 1 = 6.
//! 2. kind(None): 1.
//! 3. `if let None = o` with o = Some(2) is false: 20.
//! 4. Some(None) against `Some(None)` then `Some(Some(v))`: Some(Some(4)) gives 4.
//! A WRONG builder that reads `None` as a binding takes the first arm every time: it exits 1
//! (kind(Some(5)) = 1). This is the bug found on 2026-11-16 in the new lowering.

fn kind(o: Option<u32>) -> u32 {
    match o {
        None => 1,
        Some(n) => n + 1,
    }
}

fn nested(o: Option<Option<u32>>) -> u32 {
    match o {
        None => 100,
        Some(None) => 200,
        Some(Some(v)) => v,
    }
}

fn main() -> std::entry::Status {
    let o = Some(2u32);
    let iflet = if let None = o { 10 } else { 20 };
    let status = if kind(Some(5)) != 6 {
        1
    } else if kind(None) != 1 {
        2
    } else if iflet != 20 {
        3
    } else if nested(Some(Some(4))) != 4 || nested(Some(None)) != 200 || nested(None) != 100 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
