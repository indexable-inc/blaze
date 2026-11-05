//@ expect: reject E0412
//@ edition: 2021
#![crate_type = "lib"]

pub mod m {
    pub struct Present;
}

pub fn f(_: m::Present, _: m::Missing) {}
