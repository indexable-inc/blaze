//@ expect: accept
//@ crate: bin
//@ run: panic
//@ edition: 2021
// `-m` on i8::MIN under `#[warn(arithmetic_overflow)]`: a warning only, and the negation panics at run time
#[warn(arithmetic_overflow)]
fn main() {
    let m = i8::MIN;
    let x = -m;
    println!("{}", x);
}
