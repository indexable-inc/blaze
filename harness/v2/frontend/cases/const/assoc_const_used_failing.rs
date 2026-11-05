//@ expect: reject E0080
//@ crate: bin
//@ edition: 2021
// the same failing associated constant, used from main: evaluated where used, E0080 inside the const fn
const fn add(a: u8, b: u8) -> u8 {
    a + b
}

struct S;

impl S {
    const BAD: u8 = add(255, 1);
}

fn main() {
    println!("{}", S::BAD);
}
