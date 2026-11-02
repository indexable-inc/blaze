//@ expect: accept
//@ edition: 2021
//@ aux-crate: arith=arith.rs
fn main() { println!("{} {} {} {}", arith::add(2, 3), arith::mul(4, 5), arith::NINE, arith::TWELVE); }
