//@ expect: reject E0370
//@ crate: lib
//@ edition: 2021
// an implicit discriminant after `A = 255` in repr(u8): rustc's E0370, not E0080
#![crate_type = "lib"]
#[repr(u8)]
pub enum E {
    A = 255,
    B,
}
