//@ expect: accept
//@ edition: 2021
//@ aux-crate: implargs=implargs.rs
fn main() { println!("{} {}", implargs::join_all([1, 2, 3]), implargs::join_all(implargs::evens())); }
