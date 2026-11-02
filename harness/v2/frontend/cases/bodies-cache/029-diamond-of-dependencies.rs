//@ expect: accept
//@ edition: 2021
//@ aux-crate: left=left.rs
//@ aux-crate: right=right.rs
fn main() { println!("{} {}", left::left(), right::right()); }
