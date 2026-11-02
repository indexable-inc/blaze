//@ expect: accept
//@ edition: 2021
// An opaque return type leaks its hidden type's auto traits: `impl Display` hiding `u32` is Send.
use std::fmt::Display;

fn make() -> impl Display {
    5u32
}

fn require_send<T: Send>(t: T) -> T {
    t
}

fn main() {
    let v = require_send(make());
    println!("{v}");
}
