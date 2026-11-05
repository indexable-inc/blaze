//@ expect: accept
//@ edition: 2021
// An autoref at the first step wins over a by-value candidate at the next.
use std::ops::Deref;

#[derive(Clone, Copy)]
struct Inner;
struct Outer(Inner);

impl Deref for Outer {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

trait Who {
    fn who(self) -> &'static str;
}
impl Who for &Outer {
    fn who(self) -> &'static str {
        "outer by ref"
    }
}
impl Who for Inner {
    fn who(self) -> &'static str {
        "inner by value"
    }
}

fn main() {
    let o = Outer(Inner);
    println!("{}", o.who());
    println!("{}", Inner.who());
}
