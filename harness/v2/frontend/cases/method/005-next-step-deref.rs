//@ expect: accept
//@ edition: 2021
// Nothing at the first step: the probe goes on to the `Deref` target.
use std::ops::Deref;

struct Inner;
struct Outer(Inner);

impl Deref for Outer {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl Inner {
    fn hello(&self) -> &'static str {
        "inner"
    }
}

fn main() {
    let o = Outer(Inner);
    println!("{}", o.hello());
}
