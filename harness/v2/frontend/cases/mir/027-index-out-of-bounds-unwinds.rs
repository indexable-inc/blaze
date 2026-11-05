//@ expect: accept
use std::{hint::black_box, panic};

struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn main() {
    let caught = panic::catch_unwind(|| {
        let items = vec![1, 2, 3];
        let _a = D("a");
        let first = items[black_box(0)];
        let _b = D("b");
        let missing = items[black_box(7)];
        println!("never {first} {missing}");
    });
    println!("caught: {}", caught.is_err());
    let slice = panic::catch_unwind(|| {
        let arr = [D("e0"), D("e1")];
        let _c = D("c");
        let name = arr[black_box(2)].0;
        println!("never {name}");
    });
    println!("caught: {}", slice.is_err());
}
