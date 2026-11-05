//@ expect: reject E0277
//@ edition: 2021
// `&mut T` is never Copy.
fn need_copy<T: Copy>(_: T) {}

fn main() {
    let mut x = 1;
    need_copy(&mut x);
}
