//@ expect: reject E0277
//@ edition: 2021
// The same associated opaque with an `Rc` behind the impl's opaque: not Send.
use std::fmt::Display;
use std::rc::Rc;

trait Make {
    fn make(&self) -> impl Display;
}

struct S;

impl Make for S {
    fn make(&self) -> impl Display {
        Rc::new(3u8)
    }
}

fn require_send<T: Send>(t: T) -> T {
    t
}

fn main() {
    let _v = require_send(S.make());
}
