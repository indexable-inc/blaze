//@ expect: reject E0277
//@ edition: 2021
// An enum where only one variant holds an `Rc`: not Send.
use std::rc::Rc;

enum Value {
    Empty,
    Int(i64),
    Shared(Rc<str>),
}

fn is_send<T: Send>() {}

fn main() {
    is_send::<Value>();
}
