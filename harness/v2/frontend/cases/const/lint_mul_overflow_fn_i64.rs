//@ expect: reject arithmetic_overflow
//@ crate: bin
//@ edition: 2021
// `a * 2` on i64 inside a fn body with `a = 2^62` held in a local: arithmetic_overflow
fn main() {
    let a: i64 = 4611686018427387904;
    let b = a * 2;
    println!("{}", b);
}
