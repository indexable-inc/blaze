//@ expect: reject arithmetic_overflow
//@ crate: bin
//@ edition: 2021
// `255 + 1` on u8 inside a fn body is rustc's deny-by-default arithmetic_overflow lint, not E0080
fn main() {
    let x: u8 = 255 + 1;
    println!("{}", x);
}
