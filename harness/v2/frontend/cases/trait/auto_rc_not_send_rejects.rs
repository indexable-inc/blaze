//@ expect: reject E0277
//@ edition: 2021
// `Rc` has `impl !Send`: a struct of an `Rc` is not Send.
use std::rc::Rc;

struct Shared {
    name: Rc<str>,
}

fn is_send<T: Send>() {}

fn main() {
    is_send::<Shared>();
}
