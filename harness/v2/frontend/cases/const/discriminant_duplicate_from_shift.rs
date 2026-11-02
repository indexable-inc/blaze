//@ expect: reject E0081
//@ crate: lib
//@ edition: 2021
// `A = 1 << 3` and `B = 8` evaluate to the same discriminant: E0081
#![crate_type = "lib"]
pub enum E {
    A = 1 << 3,
    B = 8,
}
