//@ expect: accept
//@ edition: 2021
// `Cell<T>` and `RefCell<T>` are Send when `T` is.
use std::cell::{Cell, RefCell};

fn is_send<T: Send>() -> &'static str {
    "send"
}

fn main() {
    let r = RefCell::new(vec![1u8, 2]);
    r.borrow_mut().push(3);
    let c = Cell::new(String::from("cell"));
    println!("{} {}", is_send::<RefCell<Vec<u8>>>(), is_send::<Cell<String>>());
    println!("{} {}", r.borrow().len(), c.take());
}
