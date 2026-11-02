//@ expect: accept
//@ edition: 2021
//@ aux-crate: arith=arith.rs
const N: usize = arith::square(2) as usize;
fn main() { let a = [1u8; N]; println!("{}", a.len()); }
