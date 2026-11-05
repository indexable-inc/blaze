//@ expect: reject E0277
//@ edition: 2021
// An `impl Send` return whose hidden type is `Rc<i32>`: the opaque's bound fails.
use std::rc::Rc;

fn make() -> impl Send {
    Rc::new(1)
}

fn main() {
    let _r = make();
}
