//@ expect: accept
//@ edition: 2021
//@ aux-crate: generic=generic.rs
fn main() { println!("{} {} {:?}", generic::largest(&[3, 9, 2]), generic::largest(&[1.5, 0.5]), generic::swap_pair((1, "a"))); }
