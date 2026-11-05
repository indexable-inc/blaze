//@ expect: reject E0277
//@ edition: 2021
// A static's type must be Sync: `Cell<i32>` is not.
use std::cell::Cell;

static COUNTER: Cell<i32> = Cell::new(0);

fn main() {
    COUNTER.set(1);
}
