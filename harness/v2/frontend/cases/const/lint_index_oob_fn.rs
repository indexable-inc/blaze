//@ expect: reject unconditional_panic
//@ crate: bin
//@ edition: 2021
// indexing a 3-element array at 5 inside a fn body: unconditional_panic
fn main() {
    let a = [1u8, 2, 3];
    let x = a[5];
    println!("{}", x);
}
