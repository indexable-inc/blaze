//@ expect: reject E0034
//@ edition: 2021
// The same ambiguity through a path.
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
    S::go(&S);
}
