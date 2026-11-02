//@ expect: reject E0599
//@ edition: 2021
// Autoderef stops at a raw pointer.
struct S;
impl S {
    fn hi(&self) {}
}

fn main() {
    let s = S;
    let p: *const S = &s;
    p.hi();
}
