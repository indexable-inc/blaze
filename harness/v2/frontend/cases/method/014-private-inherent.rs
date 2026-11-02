//@ expect: reject E0624
//@ edition: 2021
// Only a private inherent method applies: E0624.
mod m {
    pub struct S;
    impl S {
        fn secret(&self) -> u8 {
            1
        }
    }
}

fn main() {
    let s = m::S;
    println!("{}", s.secret());
}
