//@ expect: accept
use std::panic;

struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn boom() -> D {
    if std::hint::black_box(true) {
        panic!("argument");
    }
    D("never")
}

fn take(_a: D, _b: D, _c: D) {
    println!("taken");
}

fn main() {
    let caught = panic::catch_unwind(|| {
        let _outer = D("outer");
        take(D("first"), D("second"), boom());
    });
    println!("caught: {}", caught.is_err());
}
