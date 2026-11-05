//@ expect: accept
//@ edition: 2015
// `derive(CoercePointee)` on a 2015 item whose predicates name `async`, `dyn` and `gen`, which
// are names in 2015. rustc's impls clone the item's predicates, which keep their edition, so the
// names stay names in the impls.
#![feature(derive_coerce_pointee)]

use std::marker::{CoercePointee, PhantomData};

mod async {
    pub trait Tr<X: ?Sized> {}
    impl<X: ?Sized, Y: ?Sized> Tr<X> for Y {}
}

mod dyn {
    pub trait Tr<X: ?Sized> {}
    impl<X: ?Sized, Y: ?Sized> Tr<X> for Y {}
}

mod gen {
    pub trait Tr<X: ?Sized> {}
    impl<X: ?Sized, Y: ?Sized> Tr<X> for Y {}
}

#[derive(CoercePointee)]
#[repr(transparent)]
struct Ptr<'a, #[pointee] T: ?Sized + async::Tr<T>, U: gen::Tr<T>>(&'a T, PhantomData<U>)
where
    U: dyn::Tr<T> + async::Tr<u8>,
    T: gen::Tr<U>,
    &'a T: dyn::Tr<u8>;

fn main() {
    let a = [1u8, 2, 3, 4];
    let p: Ptr<[u8; 4], u8> = Ptr(&a, PhantomData);
    let q: Ptr<[u8], u8> = p;
    assert!(q.0.len() == 4);
}
