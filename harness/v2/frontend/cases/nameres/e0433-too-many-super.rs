//@ expect: reject E0433
//@ edition: 2021
fn f() {}

fn main() {
    f();
    super::f();
}
