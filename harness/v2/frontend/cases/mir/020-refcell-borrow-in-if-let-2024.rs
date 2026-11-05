//@ expect: accept
//@ edition: 2024
use std::{cell::RefCell, panic};

fn main() {
    let cell = RefCell::new(None::<u32>);
    let caught = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        if let Some(_) = *cell.borrow() {
            println!("some");
        } else {
            *cell.borrow_mut() = Some(1);
        }
    }));
    println!("caught: {} {:?}", caught.is_err(), cell.try_borrow().map(|it| *it).ok());
}
