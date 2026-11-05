//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MAX + 1` on i64 in a constant overflows
#![crate_type = "lib"]
pub const MAX: i64 = 9223372036854775807;
pub const X: i64 = MAX + 1;
