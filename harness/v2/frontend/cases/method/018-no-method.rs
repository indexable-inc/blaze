//@ expect: reject E0599
//@ edition: 2021
// No method of the name anywhere.
struct S;

fn main() {
    S.nothing();
}
