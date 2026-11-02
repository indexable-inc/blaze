// A static in a generic function is one static for every instance (the Reference, "Statics &
// generics"), so sharing the function's code changes nothing: the count goes on across types.
use std::sync::atomic::{AtomicUsize, Ordering};

struct P;
struct Q;

fn bump<T>() -> usize {
    static N: AtomicUsize = AtomicUsize::new(0);
    N.fetch_add(1, Ordering::Relaxed) + 1
}

fn address<T>() -> usize {
    static ONE: u8 = 1;
    &ONE as *const u8 as usize
}

fn main() {
    println!("{}", bump::<P>());
    println!("{}", bump::<Q>());
    println!("{}", bump::<&P>());
    println!("{}", bump::<&Q>());
    println!("{}", address::<P>() == address::<Q>());
    let _ = (P, Q);
}
