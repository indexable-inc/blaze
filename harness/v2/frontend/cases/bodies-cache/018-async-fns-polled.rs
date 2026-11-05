//@ expect: accept
//@ edition: 2021
//@ aux-crate: asyncs=asyncs.rs
fn main() { println!("{}", asyncs::block_on(asyncs::chain(5))); }
