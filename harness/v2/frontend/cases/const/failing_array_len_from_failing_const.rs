//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// an array length that reads a failing constant: the constant's error is the one reported
#![crate_type = "lib"]
pub const N: usize = 1 << 64;
pub fn f(a: [u8; N]) -> usize {
    a.len()
}
