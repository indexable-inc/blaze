//@ expect: accept
//@ edition: 2024
use std::cell::RefCell;

fn length() -> usize {
    let cell = RefCell::new(String::from("tail"));
    cell.borrow().len()
}

fn main() {
    println!("{}", length());
}
