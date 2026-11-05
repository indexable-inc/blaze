//@ expect: reject E0425
//@ edition: 2021
#![crate_type = "lib"]

pub struct S {
    x: u32,
}

impl S {
    pub fn get(&self) -> u32 {
        x
    }
}
