//@ expect: accept
//@ edition: 2021
mod a {
    pub fn f() -> u8 {
        1
    }
    pub fn g() -> u8 {
        3
    }
}

use a::*;

fn f() -> u8 {
    2
}

fn main() {
    println!("{} {}", f(), g());
}
