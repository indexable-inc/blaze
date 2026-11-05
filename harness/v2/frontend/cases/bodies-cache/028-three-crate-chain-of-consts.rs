//@ expect: accept
//@ edition: 2021
//@ aux-crate: chain_a=chain_a.rs
fn main() { println!("{:?} {} {}", chain_a::a_array(), chain_a::a_value(), chain_a::names()); }
