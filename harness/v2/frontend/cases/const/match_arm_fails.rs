//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// a `match` in a constant whose taken arm overflows; the arm not taken would have been fine
#![crate_type = "lib"]
pub const N: u8 = 200;
pub const X: u8 = match N {
    0..=100 => N + 1,
    _ => N * 2,
};
