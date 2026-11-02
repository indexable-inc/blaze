//@ expect: reject E0624
//@ edition: 2021
// The same call is fine inside the module and E0624 outside it.
mod m {
    pub struct S;
    impl S {
        fn secret(&self) -> u8 {
            7
        }
    }
    pub fn inside() -> u8 {
        S.secret()
    }
}

fn main() {
    println!("{}", m::inside() + m::S.secret());
}
