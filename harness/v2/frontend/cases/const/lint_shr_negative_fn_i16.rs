//@ expect: reject arithmetic_overflow
//@ crate: bin
//@ edition: 2021
// `1i16 >> -1` inside a fn body: a negative amount is out of range, arithmetic_overflow
fn main() {
    let x = 1i16 >> -1;
    println!("{}", x);
}
