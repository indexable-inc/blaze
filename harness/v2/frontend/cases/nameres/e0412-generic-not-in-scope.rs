//@ expect: reject E0412
//@ edition: 2021
#![crate_type = "lib"]

pub struct S<T>(pub T);

impl S<u8> {
    pub fn f(_: T) {}
}
