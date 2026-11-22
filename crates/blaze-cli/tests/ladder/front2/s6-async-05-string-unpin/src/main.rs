//! s6-async-05: `Unpin` of std types that hold no pinning invariant. `Unpin` is an auto trait
//! (library/core/src/marker.rs: `pub auto trait Unpin {}`), so `String` is `Unpin` because every
//! field it holds is, down to `NonNull`'s `pattern_type!(*const T is !null)` and the raw pointer
//! (`Unpin for *const T`, marker.rs `marker_impls!`); `Vec<u8>` the same way. The doc of `Unpin`
//! says so itself: "this is only possible because `String` implements `Unpin`". Split out of
//! s6-async-02 (std census 1, class C2), whose `Join2` asks `A::Output: Unpin` of a `String`.

use std::pin::Pin;

fn need<T: Unpin>(t: T) -> T {
    t
}

fn main() {
    let s = need(String::from("unpin"));
    let mut v = need(vec![1u8, 2, 3]);
    let p = Pin::new(&mut v);
    println!("{} {}", s, p.len());
}
