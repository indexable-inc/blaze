//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// a constant calling a const fn whose `+` overflows: E0080, reported inside the const fn
#![crate_type = "lib"]
pub const fn add(a: u8, b: u8) -> u8 {
    a + b
}

pub const X: u8 = add(200, 100);
