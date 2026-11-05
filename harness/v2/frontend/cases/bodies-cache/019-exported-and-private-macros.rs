//@ expect: accept
//@ edition: 2021
//@ aux-crate: macros=macros.rs
fn main() { println!("{} {} {}", macros::sum3(), macros::squared(7), macros::square!(3 + 1)); }
