//@ expect: reject E0277
//@ edition: 2021
// A trait object in a `Sized` generic position.
fn need_sized<T>() {}

fn main() {
    need_sized::<dyn std::fmt::Debug>();
}
