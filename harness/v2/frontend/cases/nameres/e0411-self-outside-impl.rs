//@ expect: reject E0411
//@ edition: 2021
#![crate_type = "lib"]

pub fn f() -> Self {
    loop {}
}
