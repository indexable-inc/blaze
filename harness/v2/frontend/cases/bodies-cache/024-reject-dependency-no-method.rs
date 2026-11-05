//@ expect: reject E0599
//@ edition: 2021
//@ aux-crate: bad_method=bad_method.rs
fn main() { println!("{}", bad_method::broken()); }
