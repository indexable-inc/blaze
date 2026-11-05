//@ expect: reject E0277
//@ edition: 2021
// A struct holding `PhantomPinned` is not Unpin.
use std::marker::PhantomPinned;

struct SelfRef {
    data: [u8; 4],
    _pin: PhantomPinned,
}

fn is_unpin<T: Unpin>() {}

fn main() {
    is_unpin::<SelfRef>();
}
