//@ expect: accept
//@ edition: 2021
//@ aux-crate: collections=collections.rs
fn main() { println!("{:?} {} {:?}", collections::counts("abca"), collections::unique(&[1, 2, 2, 3]), collections::rotate(&[1, 2, 3, 4], 1)); }
