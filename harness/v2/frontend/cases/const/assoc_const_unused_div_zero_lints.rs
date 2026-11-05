//@ expect: reject unconditional_panic
//@ crate: bin
//@ edition: 2021
// an unused associated constant `1 / 0`: never evaluated, but the unconditional_panic lint runs on its body
struct S;

#[allow(dead_code)]
impl S {
    const BAD: i32 = 1 / 0;
}

fn main() {
    println!("ok");
}
