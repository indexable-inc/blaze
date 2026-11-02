//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// `-65536 * 65537` on i32 in a constant overflows below MIN
#![crate_type = "lib"]
pub const X: i32 = -65536 * 65537;
