//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `MAX - -1` on i128 in a constant overflows
#![crate_type = "lib"]
pub const MAX: i128 = 170141183460469231731687303715884105727;
pub const X: i128 = MAX - -1;
