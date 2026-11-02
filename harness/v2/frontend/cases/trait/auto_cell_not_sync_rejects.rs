//@ expect: reject E0277
//@ edition: 2021
// `Cell` is not Sync, and neither is a struct that holds one.
use std::cell::Cell;

struct Counter {
    hits: Cell<u32>,
}

fn is_sync<T: Sync>() {}

fn main() {
    is_sync::<Counter>();
}
