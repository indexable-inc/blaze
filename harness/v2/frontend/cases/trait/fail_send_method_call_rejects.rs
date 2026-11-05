//@ expect: reject E0277
//@ edition: 2021
// A Send bound on a method's parameter, failed by an `Rc` argument.
use std::rc::Rc;

struct Queue;

impl Queue {
    fn put<T: Send>(&self, _t: T) {}
}

fn main() {
    let q = Queue;
    q.put(Rc::new(1u8));
}
