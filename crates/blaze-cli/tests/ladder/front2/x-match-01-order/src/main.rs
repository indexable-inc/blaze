//! x-match-01: when patterns overlap, the FIRST matching arm wins.
//!
//! The Reference, "Match expressions": the value "is sequentially compared to the patterns in
//! the arms until a match is found. The first arm with a matching pattern is chosen as the
//! branch target of the match".
//! 1. 5 matches `0..=9` (arm 1) and `5` (arm 2): arm 1, giving 10.
//! 2. 12 matches `10..=19` (arm 3) and `12` (arm 4): arm 3, giving 30.
//! 3. 12 against arms written in the other order: `12` comes first: 40.
//! 4. 99 matches only `_`: 50.
//! A wrong builder that lets a later arm win, or picks the most specific, exits 1 (20).

fn first(x: u32) -> u32 {
    match x {
        0..=9 => 10,
        5 => 20,
        10..=19 => 30,
        12 => 40,
        _ => 50,
    }
}

fn swapped(x: u32) -> u32 {
    match x {
        12 => 40,
        10..=19 => 30,
        _ => 50,
    }
}

fn main() -> std::entry::Status {
    let status = if first(5) != 10 {
        1
    } else if first(12) != 30 {
        2
    } else if swapped(12) != 40 {
        3
    } else if first(99) != 50 {
        4
    } else {
        0
    };
    std::entry::Status(status)
}
