//@ expect: accept
//@ edition: 2021
// The same receiver and name pick a different trait in each module.
mod a {
    pub trait Name {
        fn name(&self) -> &'static str {
            "a"
        }
    }
    impl Name for u8 {}
    pub fn call() -> &'static str {
        1u8.name()
    }
}

mod b {
    pub trait Name {
        fn name(&self) -> &'static str {
            "b"
        }
    }
    impl Name for u8 {}
    pub fn call() -> &'static str {
        1u8.name()
    }
}

fn main() {
    println!("{} {}", a::call(), b::call());
}
