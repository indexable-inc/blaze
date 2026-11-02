//@ expect: accept
//@ edition: 2021
mod a {
    pub fn f() -> u8 {
        1
    }
}

use a::*;

macro_rules! define {
    () => {
        fn f() -> u8 {
            2
        }
    };
}

define!();

fn main() {
    println!("{}", f());
}
