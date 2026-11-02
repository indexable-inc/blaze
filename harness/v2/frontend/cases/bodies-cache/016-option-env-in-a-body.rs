//@ expect: accept
//@ edition: 2021
//@ aux-crate: envread=envread.rs
fn main() { println!("{} {}", envread::unset(), envread::label()); }
