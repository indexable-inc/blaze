//@ expect: accept
//@ edition: 2021
mod a {
    pub fn f() -> u8 {
        3
    }
}

macro_rules! import {
    ($($path:tt)*) => {
        use $($path)*;
    };
}

import!(a::f);

fn main() {
    println!("{}", f());
}
