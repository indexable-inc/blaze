//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MIN / -1` on i128 in a constant overflows
#![crate_type = "lib"]
pub const MIN: i128 = -170141183460469231731687303715884105728;
pub const X: i128 = MIN / -1;
