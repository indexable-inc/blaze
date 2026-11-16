//! x-match-03: range patterns on chars and signed integers, at their bounds.
//!
//! The Reference, "Range patterns": "Range patterns match scalar values within the range
//! defined by their bounds"; `a..=b` includes b ("inclusive range"), `a..b` excludes b
//! ("exclusive range"). Chars compare by their scalar value ('a' = 97 .. 'z' = 122).
//! 1. 'z' is in 'a'..='z': 1.   2. 'Z' is in 'A'..='Z': 2.   3. '0' is in '0'..='9': 3.
//! 4. ' ' (32) is in none: 4.   5. '{' (123, one past 'z') is in none: 4.
//! 6. -1 is in i32::MIN..=-1: 1 (a builder comparing unsigned puts it past every bound).
//! 7. 0 is in 0..10: 2.        8. 10 is not in 0..10, it is in 10..=10: 3.
//! A wrong builder that excludes an inclusive end exits 1.

fn class(c: char) -> u32 {
    match c {
        'a'..='z' => 1,
        'A'..='Z' => 2,
        '0'..='9' => 3,
        _ => 4,
    }
}

fn sign(x: i32) -> u32 {
    match x {
        i32::MIN..=-1 => 1,
        0..10 => 2,
        10..=10 => 3,
        _ => 4,
    }
}

fn main() -> std::entry::Status {
    let status = if class('z') != 1 {
        1
    } else if class('Z') != 2 {
        2
    } else if class('0') != 3 {
        3
    } else if class(' ') != 4 {
        4
    } else if class('{') != 4 {
        5
    } else if sign(-1) != 1 {
        6
    } else if sign(0) != 2 {
        7
    } else if sign(10) != 3 {
        8
    } else {
        0
    };
    std::entry::Status(status)
}
