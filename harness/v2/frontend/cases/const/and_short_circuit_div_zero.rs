//@ expect: accept
//@ crate: bin
//@ edition: 2021
// `false && (1 / ZERO == 0)`: the right side of `&&` is never evaluated, so its division by zero is no error
const ZERO: i32 = 0;
const X: bool = false && (1 / ZERO == 0);
const Y: bool = (ZERO == 0) && (10 / (ZERO + 2) == 5);
const Z: bool = (ZERO != 0) && (i32::MIN / ZERO == -1);

fn main() {
    println!("{} {} {}", X, Y, Z);
}
