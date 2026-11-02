//@ expect: accept
//@ edition: 2021
// A trait brought into scope by `use`.
mod m {
    pub trait Hello {
        fn hello(&self) -> &'static str {
            "hi"
        }
    }
    impl Hello for u8 {}
}

use m::Hello;

fn main() {
    println!("{}", 1u8.hello());
}
