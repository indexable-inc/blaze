//@ expect: reject E0080
//@ crate: lib
//@ edition: 2021
// an array length `[u8; LEN - 4]` in a field's type that underflows usize
#![crate_type = "lib"]
pub const LEN: usize = 3;
pub struct S {
    pub a: [u8; LEN - 4],
}
