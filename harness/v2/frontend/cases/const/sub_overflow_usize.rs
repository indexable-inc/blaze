//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `LEN - 1` with LEN = 0 on usize in a constant overflows
#![crate_type = "lib"]
pub const LEN: usize = 0;
pub const X: usize = LEN - 1;
