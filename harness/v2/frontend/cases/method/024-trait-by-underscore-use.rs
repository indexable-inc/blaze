//@ expect: accept
//@ edition: 2021
// `use Trait as _` brings the trait's methods into scope without its name.
mod m {
    pub trait Hello {
        fn hello(&self) -> &'static str {
            "hi"
        }
    }
    impl Hello for u8 {}
}

use m::Hello as _;

fn main() {
    println!("{}", 1u8.hello());
}
