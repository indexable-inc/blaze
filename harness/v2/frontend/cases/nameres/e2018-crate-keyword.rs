//@ expect: accept
//@ edition: 2018
mod m {
    pub mod n {
        pub fn f() -> u8 {
            crate::m::g() + 1
        }
    }
    pub fn g() -> u8 {
        self::base()
    }
    fn base() -> u8 {
        10
    }
}

use crate::m::n::f as nf;

fn main() {
    println!("{} {}", crate::m::n::f(), nf());
}
