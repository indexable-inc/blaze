//@ expect: accept
//@ edition: 2021
mod a {
    pub fn f() -> u8 {
        1
    }
}

mod b {
    pub use super::a::f;
}

use a::*;
use b::*;

fn main() {
    println!("{}", f());
}
