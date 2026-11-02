//@ expect: accept
//@ crate: bin
//@ edition: 2021
// an unused associated constant `255 + 1` under `#[allow(arithmetic_overflow)]`: no lint, and never evaluated
struct S;

#[allow(dead_code, arithmetic_overflow)]
impl S {
    const BAD: u8 = 255 + 1;
}

fn main() {
    println!("ok");
}
