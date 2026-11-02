//@ expect: reject E0277
//@ edition: 2021
// A negative impl of a user auto trait on a field's type fails the outer struct.
#![feature(auto_traits, negative_impls)]

auto trait Plain {}

struct Bad(u8);

impl !Plain for Bad {}

struct Holds {
    n: u8,
    bad: Bad,
}

fn is_plain<T: Plain>() {}

fn main() {
    is_plain::<Holds>();
}
