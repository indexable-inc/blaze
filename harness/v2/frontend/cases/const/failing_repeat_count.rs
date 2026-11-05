//@ expect: reject E0080
//@ crate: bin
//@ edition: 2021
// a repeat count `[0u8; LEN - 3]` in a fn body that underflows usize
const LEN: usize = 2;

fn main() {
    let a = [0u8; LEN - 3];
    println!("{}", a.len());
}
