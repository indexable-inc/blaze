//@ expect: reject arithmetic_overflow
//@ crate: bin
//@ edition: 2021
// `1i32 << 32` inside a fn body: arithmetic_overflow
fn main() {
    let x = 1i32 << 32;
    println!("{}", x);
}
