//@ expect: reject E0277
//@ edition: 2021
// The same generic ADT, asked with an argument that fails.
use std::rc::Rc;

struct Wrap<T> {
    inner: T,
    count: usize,
}

fn is_send<T: Send>() {}

fn main() {
    is_send::<Wrap<Rc<u8>>>();
}
