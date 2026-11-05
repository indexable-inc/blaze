//@ expect: accept
//@ edition: 2021
// A private inherent method is set aside for a visible trait method of the name.
mod m {
    pub struct S;
    impl S {
        #[allow(dead_code)]
        fn name(&self) -> &'static str {
            "private"
        }
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
    println!("{}", m::S.name());
}
