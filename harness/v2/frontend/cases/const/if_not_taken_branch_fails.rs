//@ expect: accept
//@ crate: bin
//@ edition: 2021
// an `if` in a constant whose branch not taken would divide by zero: never evaluated
const ZERO: i32 = 0;
const X: i32 = if ZERO == 0 { 42 } else { 1 / ZERO };

fn main() {
    println!("{}", X);
}
