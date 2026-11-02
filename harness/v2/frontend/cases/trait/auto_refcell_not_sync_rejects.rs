//@ expect: reject E0277
//@ edition: 2021
// `RefCell<T>` is never Sync.
use std::cell::RefCell;

fn is_sync<T: Sync>() {}

fn main() {
    is_sync::<RefCell<u8>>();
}
