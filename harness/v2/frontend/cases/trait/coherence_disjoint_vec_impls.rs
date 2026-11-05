//@ expect: accept
//@ edition: 2021
// Impls for `Vec<u8>`, `Vec<u16>` and `Option<T>` do not overlap.
#![crate_type = "lib"]

pub trait Tr {
    const N: u8;
}

impl Tr for Vec<u8> {
    const N: u8 = 1;
}
impl Tr for Vec<u16> {
    const N: u8 = 2;
}
impl<T> Tr for Option<T> {
    const N: u8 = 3;
}

pub fn sum() -> u8 {
    <Vec<u8> as Tr>::N + <Vec<u16> as Tr>::N + <Option<String> as Tr>::N
}
