//@ expect: reject "pointers cannot be cast to integers during const eval"
//@ edition: 2021
//@ issue: #1440
// One of the const-check errors #1440 names that rustc gives no code: a pointer cast to an
// integer in a `const fn`.
#![crate_type = "lib"]

pub const fn address(pointer: *const u8) -> usize {
    pointer as usize
}
