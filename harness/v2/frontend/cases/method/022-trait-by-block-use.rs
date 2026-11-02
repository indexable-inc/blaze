//@ expect: accept
//@ edition: 2021
// A trait brought into scope by a block's `use`.
mod m {
    pub trait Hello {
        fn hello(&self) -> &'static str {
            "hi"
        }
    }
    impl Hello for u8 {}
}

fn main() {
    {
        use m::Hello;
        println!("{}", 1u8.hello());
    }
}
