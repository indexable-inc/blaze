//@ expect: reject E0277
//@ edition: 2021
// A Sync bound on a method's parameter, failed by a `RefCell` argument.
use std::cell::RefCell;

struct Board;

impl Board {
    fn share<T: Sync>(&self, t: &T) -> usize {
        std::mem::size_of_val(t)
    }
}

fn main() {
    let b = Board;
    let cell = RefCell::new(0u32);
    let _n = b.share(&cell);
}
