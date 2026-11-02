//@ expect: reject E0599
//@ edition: 2021
// The trait is in scope but not implemented for the receiver.
trait T {
    fn t(&self) {}
}

struct S;
impl T for u8 {}

fn main() {
    1u8.t();
    S.t();
}
