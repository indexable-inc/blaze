//@ expect: accept
//@ edition: 2021
// A trait method by value wins over an inherent one that needs an autoref.
struct S;

impl S {
    fn f(&self) -> &'static str {
        "inherent"
    }
}

trait T {
    fn f(self) -> &'static str;
}
impl T for S {
    fn f(self) -> &'static str {
        "trait"
    }
}

fn main() {
    println!("{}", S.f());
}
