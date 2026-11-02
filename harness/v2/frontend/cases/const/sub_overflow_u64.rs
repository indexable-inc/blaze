//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `A - B` with B > A on u64 in a constant overflows
#![crate_type = "lib"]
pub const A: u64 = 3;
pub const B: u64 = 4;
pub const X: u64 = A - B;
