//@ expect: accept
//@ run: panic
//@ edition: 2021
//@ aux-crate: panics=panics.rs
fn main() {
    println!("{} {}", panics::checked_div(10, 3), panics::index(&[4, 5], 1));
    println!("{}", panics::checked_div(1, 0));
}
