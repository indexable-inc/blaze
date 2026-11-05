//@ expect: reject E0277
//@ edition: 2021
// A local must be Sized: a `str` local is rejected.
fn main() {
    let s: str = *"abc";
}
