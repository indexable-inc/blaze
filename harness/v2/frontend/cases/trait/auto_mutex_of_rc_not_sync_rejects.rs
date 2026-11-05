//@ expect: reject E0277
//@ edition: 2021
// `Mutex<T>: Sync` needs `T: Send`: `Mutex<Rc<u8>>` fails on `Rc<u8>: Send`.
use std::rc::Rc;
use std::sync::Mutex;

fn is_sync<T: Sync>() {}

fn main() {
    is_sync::<Mutex<Rc<u8>>>();
}
