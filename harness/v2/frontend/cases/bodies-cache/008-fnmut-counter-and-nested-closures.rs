//@ expect: accept
//@ edition: 2021
//@ aux-crate: closures=closures.rs
fn main() { let mut c = closures::counter(); c(); c(); println!("{} {}", c(), closures::nested(4)); }
