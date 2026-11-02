//@ expect: accept
use std::{hint::black_box, panic};

struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn divide(a: i32, b: i32) -> i32 {
    let _frame = D("divide");
    a / b
}

fn main() {
    println!("{}", divide(black_box(9), black_box(3)));
    let caught = panic::catch_unwind(|| {
        let _outer = D("outer");
        let q = divide(black_box(1), black_box(0));
        println!("never {q}");
    });
    println!("caught: {}", caught.is_err());
    let remainder = panic::catch_unwind(|| {
        let _r = D("remainder");
        black_box(i32::MIN) % black_box(-1)
    });
    println!("caught: {}", remainder.is_err());
}
