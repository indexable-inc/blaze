//@ expect: reject E0425
//@ edition: 2021
//@ aux-crate: bad_name=bad_name.rs
fn main() { println!("{}", bad_name::broken()); }
