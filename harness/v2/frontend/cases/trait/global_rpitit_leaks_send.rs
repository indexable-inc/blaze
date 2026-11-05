//@ expect: accept
//@ edition: 2021
// A return-position `impl Trait` in a trait: `<S as Make>::make`'s associated opaque, for a
// known impl, normalizes to the impl's opaque and leaks its hidden `u8`, which is Send.
use std::fmt::Display;

trait Make {
    fn make(&self) -> impl Display;
}

struct S;

impl Make for S {
    fn make(&self) -> impl Display {
        3u8
    }
}

fn require_send<T: Send>(t: T) -> T {
    t
}

fn main() {
    println!("{}", require_send(S.make()));
}
