//@ expect: accept
//@ edition: 2021
//@ aux-crate: drops=drops.rs
fn main() { drops::scoped(); let m = drops::moved(); println!("got {}", m.0); }
