//@ expect: accept
//@ edition: 2021
//@ aux-crate: consts=consts.rs
fn main() { let a: [u8; consts::LEN] = consts::TABLE; println!("{:?}", a); }
