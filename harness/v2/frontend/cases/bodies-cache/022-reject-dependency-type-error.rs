//@ expect: reject E0308
//@ edition: 2021
//@ aux-crate: bad_type=bad_type.rs
fn main() { println!("{}", bad_type::broken()); }
