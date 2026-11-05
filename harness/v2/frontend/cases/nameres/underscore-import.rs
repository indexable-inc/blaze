//@ expect: accept
//@ edition: 2021
use std::fmt::Write as _;
use std::io::Write as _;

fn main() {
    let mut s = String::new();
    write!(s, "{}-{}", 1, 2).unwrap();
    let mut out: std::vec::Vec<u8> = std::vec::Vec::new();
    write!(out, "{}", 3).unwrap();
    println!("{} {:?}", s, out);
}
