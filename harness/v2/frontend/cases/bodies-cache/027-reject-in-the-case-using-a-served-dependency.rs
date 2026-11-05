//@ expect: reject E0308
//@ edition: 2021
//@ aux-crate: arith=arith.rs
fn main() { let s: String = arith::add(1, 2); println!("{s}"); }
