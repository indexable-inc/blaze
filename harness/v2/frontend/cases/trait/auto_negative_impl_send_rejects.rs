//@ expect: reject E0277
//@ edition: 2021
// `impl !Send` on a struct of scalars, which would otherwise be Send.
#![feature(negative_impls)]

struct Token(u8);

impl !Send for Token {}

fn is_send<T: Send>() {}

fn main() {
    is_send::<Token>();
}
