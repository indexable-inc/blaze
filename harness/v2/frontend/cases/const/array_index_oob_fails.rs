//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// indexing a constant array at its length in a constant: E0080, not the unconditional_panic lint
#![crate_type = "lib"]
pub const A: [u8; 3] = [1, 2, 3];
pub const X: u8 = A[3];
