//@ expect: reject E0599
//@ edition: 2021
// The trait has the method but is not in scope.
mod m {
    pub trait Hello {
        fn hello(&self) {}
    }
    impl Hello for u8 {}
}

fn main() {
    1u8.hello();
}
