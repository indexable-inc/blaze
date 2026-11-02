//@ expect: reject E0659
//@ edition: 2021
mod a {
    macro_rules! mac {
        () => {
            1
        };
    }
    pub(crate) use mac;
}

mod b {
    macro_rules! mac {
        () => {
            2
        };
    }
    pub(crate) use mac;
}

use a::*;

macro_rules! import {
    () => {
        use b::mac;
    };
}

import!();

fn main() {
    println!("{}", mac!());
}
