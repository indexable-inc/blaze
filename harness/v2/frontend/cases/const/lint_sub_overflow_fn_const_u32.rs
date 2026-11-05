//@ expect: reject arithmetic_overflow
//@ crate: bin
//@ edition: 2021
// `ZERO - 1` on u32 inside a fn body, ZERO a constant: the lint reads the constant's value
const ZERO: u32 = 0;

fn main() {
    let x = ZERO - 1;
    println!("{}", x);
}
