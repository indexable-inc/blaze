//@ expect: accept
//@ edition: 2021
// The same receiver and name pick the private inherent method inside its module and the trait's outside.
mod m {
    pub struct S;
    impl S {
        fn name(&self) -> &'static str {
            "inherent"
        }
    }
    pub fn inside() -> &'static str {
        S.name()
    }
}

trait Name {
    fn name(&self) -> &'static str;
}
impl Name for m::S {
    fn name(&self) -> &'static str {
        "trait"
    }
}

fn main() {
    println!("{} {}", m::inside(), m::S.name());
}
