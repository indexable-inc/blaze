//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// a block constant whose `+=` on a `let mut` overflows u8
#![crate_type = "lib"]
pub const X: u8 = {
    let mut v: u8 = 250;
    v += 10;
    v
};
