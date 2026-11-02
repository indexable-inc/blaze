//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `L + R` with both operands failing constants: each is reported at its own expression, the sum adds nothing
#![crate_type = "lib"]
pub const L: u16 = 65535 * 2;
pub const R: u16 = 1 / 0;
pub const X: u16 = L + R;
