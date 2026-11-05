//@ expect: accept
//@ edition: 2021
// With `supertrait_item_shadowing` the subtrait's method shadows the supertrait's.
#![feature(supertrait_item_shadowing)]
#![allow(incomplete_features)]

trait Up {
    fn hello(&self) -> &'static str {
        "up"
    }
}
trait Down: Up {
    fn hello(&self) -> &'static str {
        "down"
    }
}

impl Up for i32 {}
impl Down for i32 {}

fn main() {
    println!("{}", 1i32.hello());
}
