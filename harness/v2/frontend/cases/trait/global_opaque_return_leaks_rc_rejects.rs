//@ expect: reject E0277
//@ edition: 2021
// The same leak with an `Rc` behind the opaque: `require_send` fails.
use std::fmt::Display;
use std::rc::Rc;

fn make() -> impl Display {
    Rc::new(5u32)
}

fn require_send<T: Send>(t: T) -> T {
    t
}

fn main() {
    let _v = require_send(make());
}
