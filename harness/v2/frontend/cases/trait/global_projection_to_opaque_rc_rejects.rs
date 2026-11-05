//@ expect: reject E0277
//@ edition: 2021
// The same projection, normalizing to an opaque hiding `Rc<u32>`: `F::Output: Send` fails.
use std::fmt::Display;
use std::rc::Rc;

fn mk() -> impl Display {
    Rc::new(7u32)
}

fn run<F: FnOnce()>(f: F) -> F::Output
where
    F::Output: Send,
{
    f()
}

fn main() {
    let _r = run(mk);
}
