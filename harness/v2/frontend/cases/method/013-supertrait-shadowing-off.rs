//@ expect: reject E0034
//@ edition: 2021
// Without the feature the subtrait's and supertrait's methods are ambiguous.
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
