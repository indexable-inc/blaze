//@ expect: accept
#![feature(coerce_unsized)]

use core::ops::CoerceUnsized;

struct Wrap<T>(T);

fn convert<T, U>(t: T) -> U
where
    T: CoerceUnsized<U>,
{
    t
}

fn main() {
    let a = [1u32, 2];
    let s: &[u32] = convert(&a);
    assert!(s.len() == 2);
    let w = Wrap(5u32);
    assert!(w.0 == 5);
}
