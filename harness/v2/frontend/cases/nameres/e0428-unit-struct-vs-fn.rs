//@ expect: reject E0428
//@ edition: 2021
struct S;

#[allow(non_snake_case)]
fn S() {}

fn main() {}
