//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MIN % -1` on i64 in a constant overflows, although the remainder would be 0
#![crate_type = "lib"]
pub const MIN: i64 = -9223372036854775808;
pub const X: i64 = MIN % -1;
