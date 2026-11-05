//@ expect: reject E0119
//@ edition: 2021
// Two impls of one trait for one type.
#![crate_type = "lib"]

pub trait Tr {}

impl Tr for u8 {}
impl Tr for u8 {}
