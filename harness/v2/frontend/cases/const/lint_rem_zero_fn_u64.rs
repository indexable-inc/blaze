//@ expect: reject unconditional_panic
//@ crate: bin
//@ edition: 2021
// `a % Z` on u64 inside a fn body, Z a zero constant: unconditional_panic
const Z: u64 = 0;

fn main() {
    let a: u64 = 17;
    let x = a % Z;
    println!("{}", x);
}
