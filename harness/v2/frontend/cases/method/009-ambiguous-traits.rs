//@ expect: reject E0034
//@ edition: 2021
// Two traits in scope apply at the same step: ambiguity.
trait A {
    fn go(&self) {}
}
trait B {
    fn go(&self) {}
}

struct S;
impl A for S {}
impl B for S {}

fn main() {
    S.go();
}
