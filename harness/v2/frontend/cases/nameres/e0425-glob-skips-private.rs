//@ expect: reject E0425
//@ edition: 2021
mod a {
    fn hidden() {}
    pub fn shown() {
        hidden();
    }
}

use a::*;

fn main() {
    shown();
    hidden();
}
