//@ expect: reject E0499
//@ edition: 2021
//@ aux-crate: bad_borrow=bad_borrow.rs
fn main() { println!("{}", bad_borrow::broken()); }
