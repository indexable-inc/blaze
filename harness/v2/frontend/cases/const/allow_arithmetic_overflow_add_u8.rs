//@ expect: accept
//@ crate: bin
//@ run: panic
//@ edition: 2021
// `255 + 1` in a fn body under `#[allow(arithmetic_overflow)]` builds, and panics when the line runs
#[allow(arithmetic_overflow)]
fn main() {
    println!("before");
    let x: u8 = 255 + 1;
    println!("{}", x);
}
