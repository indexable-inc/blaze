//@ expect: accept
//@ edition: 2021
// A private method is visible from its own module.
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
    println!("{}", m::inside());
}
