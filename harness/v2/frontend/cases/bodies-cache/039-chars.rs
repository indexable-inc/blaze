//@ expect: accept
//@ edition: 2021
//@ aux-crate: chars=chars.rs
fn main() { println!("{} {}", chars::caesar("hello, zed", 3), chars::digits("a1b22c3")); }
