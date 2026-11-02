//@ expect: accept
//@ edition: 2021
//@ aux-crate: generic=generic.rs
fn main() { println!("{} {}", generic::sum_array([1, 2, 3]), generic::sum_array([10; 4])); }
