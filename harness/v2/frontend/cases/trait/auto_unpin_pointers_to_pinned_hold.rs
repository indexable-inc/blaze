//@ expect: accept
//@ edition: 2021
// References, raw pointers and Box are Unpin through core's and alloc's impls, whatever they
// point to: the structural rule is never reached.
use std::marker::PhantomPinned;

fn is_unpin<T: Unpin>() -> u32 {
    1
}

fn main() {
    let n = is_unpin::<&'static PhantomPinned>()
        + is_unpin::<&'static mut PhantomPinned>()
        + is_unpin::<*const PhantomPinned>()
        + is_unpin::<*mut PhantomPinned>()
        + is_unpin::<Box<PhantomPinned>>();
    println!("{n}");
}
