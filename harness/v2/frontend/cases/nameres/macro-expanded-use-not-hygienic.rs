//@ expect: accept
//@ edition: 2021
mod a {
    pub fn g() -> u8 {
        4
    }
}

macro_rules! import_g {
    () => {
        use crate::a::g;
    };
}

import_g!();

fn main() {
    println!("{}", g());
}
