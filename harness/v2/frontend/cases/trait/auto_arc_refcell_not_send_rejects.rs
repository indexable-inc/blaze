//@ expect: reject E0277
//@ edition: 2021
// `Arc<T>: Send` needs `T: Send + Sync`: `Arc<RefCell<i32>>` fails on `RefCell<i32>: Sync`.
use std::cell::RefCell;
use std::sync::Arc;

fn is_send<T: Send>() {}

fn main() {
    is_send::<Arc<RefCell<i32>>>();
}
