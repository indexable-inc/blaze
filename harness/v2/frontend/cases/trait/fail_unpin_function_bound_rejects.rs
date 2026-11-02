//@ expect: reject E0277
//@ edition: 2021
// An Unpin function bound, failed by a generic wrapper over `PhantomPinned`.
use std::marker::PhantomPinned;

struct Wrap<T>(u8, T);

fn need_unpin<T: Unpin>(_t: &T) {}

fn main() {
    let w = Wrap(1, PhantomPinned);
    need_unpin(&w);
}
