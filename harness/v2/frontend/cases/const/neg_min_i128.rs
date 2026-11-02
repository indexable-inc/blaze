//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `-i128::MIN`, MIN through the primitive's associated constant, overflows
#![crate_type = "lib"]
pub const X: i128 = -i128::MIN;
