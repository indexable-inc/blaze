//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `L / ZERO` with L failing: the operand fails before the division by zero is checked, so only L's error is reported
#![crate_type = "lib"]
pub const L: i32 = 2147483647 + 1;
pub const ZERO: i32 = 0;
pub const X: i32 = L / ZERO;
