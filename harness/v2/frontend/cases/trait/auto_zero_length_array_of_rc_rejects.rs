//@ expect: reject E0277
//@ edition: 2021
// An array's constituent is its element whatever the length: `[Rc<u8>; 0]` is not Send.
use std::rc::Rc;

fn is_send<T: Send>() {}

fn main() {
    is_send::<[Rc<u8>; 0]>();
}
