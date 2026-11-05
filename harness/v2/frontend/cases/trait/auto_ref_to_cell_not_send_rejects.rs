//@ expect: reject E0277
//@ edition: 2021
// `&Cell<i32>: Send` needs `Cell<i32>: Sync`, which does not hold.
use std::cell::Cell;

fn is_send<T: Send>() {}

fn main() {
    is_send::<&Cell<i32>>();
}
