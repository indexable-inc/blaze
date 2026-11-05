//@ expect: reject unconditional_panic
//@ crate: bin
//@ edition: 2021
// `i128::MIN / -1` inside a fn body: division overflow is unconditional_panic, not arithmetic_overflow
fn main() {
    let x = i128::MIN / -1;
    println!("{}", x);
}
