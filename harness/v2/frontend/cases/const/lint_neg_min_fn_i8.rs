//@ expect: reject arithmetic_overflow
//@ crate: bin
//@ edition: 2021
// `-m` with `m = i8::MIN` inside a fn body: arithmetic_overflow
fn main() {
    let m = i8::MIN;
    let x = -m;
    println!("{}", x);
}
