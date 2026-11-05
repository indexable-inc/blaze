//@ expect: reject E0277
//@ edition: 2021
// `Pin::new` needs the pointee to be Unpin: a struct with `PhantomPinned` is not.
use std::marker::PhantomPinned;
use std::pin::Pin;

struct Pinned {
    n: u8,
    _pin: PhantomPinned,
}

fn main() {
    let mut p = Pinned { n: 1, _pin: PhantomPinned };
    let _pinned = Pin::new(&mut p);
}
