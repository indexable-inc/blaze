//@ expect: accept
//@ edition: 2021
// A user impl of Unpin replaces the structural answer: the PhantomPinned field no longer matters.
use std::marker::PhantomPinned;
use std::pin::Pin;

struct Pinned {
    n: u8,
    _pin: PhantomPinned,
}

impl Unpin for Pinned {}

fn is_unpin<T: Unpin>() -> &'static str {
    "unpin"
}

fn main() {
    let mut p = Pinned { n: 9, _pin: PhantomPinned };
    let pinned = Pin::new(&mut p);
    println!("{} {}", is_unpin::<Pinned>(), pinned.n);
}
