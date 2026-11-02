//@ expect: reject E0597
//@ edition: 2021
use std::cell::RefCell;

fn length() -> usize {
    let cell = RefCell::new(String::from("tail"));
    cell.borrow().len() //~ ERROR E0597
}

fn main() {
    println!("{}", length());
}
