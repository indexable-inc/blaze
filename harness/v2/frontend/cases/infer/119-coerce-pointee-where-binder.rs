//@ expect: accept
// `derive(CoercePointee)` on an item whose `where` predicate names the pointee only in its bound,
// under a `for<'x>` binder. rustc's impls copy the predicate whole, binder included, with `__S`
// for the pointee; without the binder `'x` would be undeclared in the impls.
#![feature(derive_coerce_pointee)]

use std::marker::{CoercePointee, PhantomData};

trait Tr<X: ?Sized> {}
impl<X: ?Sized, Y: ?Sized> Tr<X> for Y {}

#[derive(CoercePointee)]
#[repr(transparent)]
struct Ptr<'a, #[pointee] T: ?Sized, U>(&'a T, PhantomData<U>)
where
    for<'x> Vec<&'x U>: Tr<T>,
    Vec<U>: Send;

fn main() {
    let a = [1u8, 2, 3, 4];
    let p: Ptr<[u8; 4], u8> = Ptr(&a, PhantomData);
    let q: Ptr<[u8], u8> = p;
    assert!(q.0.len() == 4);
}
