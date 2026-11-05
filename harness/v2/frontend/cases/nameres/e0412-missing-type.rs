//@ expect: reject E0412
//@ edition: 2021
#![crate_type = "lib"]

pub struct Known;

pub fn f(_: Known, _: Unknown) {}
