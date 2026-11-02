//@ expect: reject E0405
//@ edition: 2021
#![crate_type = "lib"]

pub trait Real {}

impl Real for u8 {}
impl Nope for u16 {}
