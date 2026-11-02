//@ expect: accept
#![feature(coerce_unsized, unsize)]

use core::marker::Unsize;
use core::ops::CoerceUnsized;

struct Ptr<'a, T: ?Sized>(&'a T);

impl<'a, T: ?Sized + Unsize<U>, U: ?Sized> CoerceUnsized<Ptr<'a, U>> for Ptr<'a, T> {}

trait Named {
    fn id(&self) -> u32;
}

struct One;

impl Named for One {
    fn id(&self) -> u32 {
        1
    }
}

fn main() {
    let a = [1u32, 2, 3];
    let p: Ptr<[u32]> = Ptr(&a);
    assert!(p.0.len() == 3);
    let q: Ptr<dyn Named> = Ptr(&One);
    assert!(q.0.id() == 1);
}
