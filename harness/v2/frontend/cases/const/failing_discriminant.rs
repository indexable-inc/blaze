//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// an explicit discriminant `200 + 100` that overflows its repr(u8)
#![crate_type = "lib"]
#[repr(u8)]
pub enum E {
    A = 200 + 100,
    B = 1,
}
