//@ expect: accept
//@ edition: 2021
mod a {
    pub fn f() -> u8 {
        1
    }
    pub fn only_a() -> u8 {
        10
    }
}

mod b {
    pub fn f() -> u8 {
        2
    }
    pub fn only_b() -> u8 {
        20
    }
}

use a::*;
use b::*;

fn main() {
    println!("{}", only_a() + only_b());
}
