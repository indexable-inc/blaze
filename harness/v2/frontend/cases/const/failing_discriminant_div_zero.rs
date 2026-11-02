//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// an explicit discriminant of the default isize repr that divides by a zero constant
#![crate_type = "lib"]
pub const Z: isize = 0;
pub enum E {
    A = 1,
    B = 8 / Z,
}
