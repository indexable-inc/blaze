//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `64 * 2` on i8 in a constant overflows MAX
#![crate_type = "lib"]
pub const X: i8 = 64 * 2;
