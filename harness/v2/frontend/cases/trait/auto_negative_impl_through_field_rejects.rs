//@ expect: reject E0277
//@ edition: 2021
// A negative impl on a field's type makes the outer struct not Send.
#![feature(negative_impls)]

struct Token(u8);

impl !Send for Token {}

struct Outer {
    n: u32,
    inner: (u8, Token),
}

fn is_send<T: Send>() {}

fn main() {
    is_send::<Outer>();
}
