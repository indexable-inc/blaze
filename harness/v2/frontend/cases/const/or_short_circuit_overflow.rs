//@ expect: accept
//@ crate: bin
//@ edition: 2021
// `true || (MAX + ONE == 0)`: the right side of `||` is never evaluated, so its overflow is no error
const MAX: u8 = 255;
const ONE: u8 = 1;
const X: bool = true || (MAX + ONE == 0);
const Y: bool = false || ONE > 0;
const Z: bool = (MAX == 255) || (MAX << 8 == 0);

fn main() {
    println!("{} {} {}", X, Y, Z);
}
