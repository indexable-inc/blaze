//@ expect: reject E0119
//@ edition: 2021
// A blanket impl over `T: Copy` and an impl for `String`: `String: Copy` is not knowable from
// this crate (std may add the impl), so the impls may overlap.
#![crate_type = "lib"]

pub trait Tr {}

impl<T: Copy> Tr for T {}
impl Tr for String {}
