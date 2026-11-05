//@ expect: reject E0603
//@ edition: 2021
mod m {
    struct S;
}

use m::S;

fn main() {}
