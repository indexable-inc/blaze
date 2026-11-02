//@ expect: reject E0277
//@ edition: 2021
// The same recursive list with one `Rc` field: the cycle would hold, the leaf fails.
use std::rc::Rc;

struct Node {
    value: Rc<i32>,
    next: Option<Box<Node>>,
}

fn is_send<T: Send>() {}

fn main() {
    is_send::<Node>();
}
