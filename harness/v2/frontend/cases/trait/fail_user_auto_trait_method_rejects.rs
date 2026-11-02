//@ expect: reject E0277
//@ edition: 2021
// A user auto trait bound on a method, failed through a field with a negative impl.
#![feature(auto_traits, negative_impls)]

auto trait Plain {}

struct Bad;

impl !Plain for Bad {}

struct Holds {
    n: u8,
    bad: Bad,
}

struct Sink;

impl Sink {
    fn take<T: Plain>(&self, _t: T) {}
}

fn main() {
    Sink.take(Holds { n: 1, bad: Bad });
}
