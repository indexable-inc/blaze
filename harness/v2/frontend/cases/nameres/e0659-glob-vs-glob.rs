//@ expect: reject E0659
//@ edition: 2021
mod a {
    pub fn f() {}
}

mod b {
    pub fn f() {}
}

use a::*;
use b::*;

fn main() {
    f();
}
