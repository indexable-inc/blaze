//@ expect: accept
//@ crate: bin
//@ edition: 2021
// an unused associated constant that would fail (through a const fn the lint cannot see into) is never evaluated
const fn add(a: u8, b: u8) -> u8 {
    a + b
}

struct S;

#[allow(dead_code)]
impl S {
    const BAD: u8 = add(255, 1);
    const GOOD: u8 = add(250, 5);
}

fn main() {
    println!("{}", S::GOOD);
}
