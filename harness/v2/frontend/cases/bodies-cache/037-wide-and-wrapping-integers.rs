//@ expect: accept
//@ edition: 2021
//@ aux-crate: wide=wide.rs
fn main() { println!("{} {} {:?} {:?} {:?}", wide::big(1 << 40), wide::wrap(100), wide::checked(5000), wide::checked(2), wide::bits(0b1011)); }
