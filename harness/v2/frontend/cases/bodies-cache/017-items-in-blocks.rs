//@ expect: accept
//@ edition: 2021
//@ aux-crate: blocks=blocks.rs
fn main() { println!("{} {}", blocks::with_inner(), blocks::block_value()); }
