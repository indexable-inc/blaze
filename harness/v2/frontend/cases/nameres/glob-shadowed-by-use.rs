//@ expect: accept
//@ edition: 2021
mod a {
    pub fn f() -> u8 {
        1
    }
}

mod b {
    pub fn f() -> u8 {
        2
    }
}

use a::*;
use b::f;

fn main() {
    println!("{}", f());
}
