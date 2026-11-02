//@ expect: reject E0119
//@ edition: 2021
// `impl<T> Tr for Vec<T>` overlaps `impl Tr for Vec<u8>`.
#![crate_type = "lib"]

pub trait Tr {}

impl<T> Tr for Vec<T> {}
impl Tr for Vec<u8> {}
