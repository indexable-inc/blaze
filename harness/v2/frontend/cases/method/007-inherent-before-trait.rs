//@ expect: accept
//@ edition: 2021
// At one step and adjustment an inherent method wins over a trait's.
struct S;

impl S {
    fn f(&self) -> &'static str {
        "inherent"
    }
}

trait T {
    fn f(&self) -> &'static str;
}
impl T for S {
    fn f(&self) -> &'static str {
        "trait"
    }
}

fn main() {
    println!("{}", S.f());
    println!("{}", T::f(&S));
}
