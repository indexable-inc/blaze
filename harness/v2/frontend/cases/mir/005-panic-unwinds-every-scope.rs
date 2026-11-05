//@ expect: accept
use std::panic;

struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn boom(at: u32) {
    if std::hint::black_box(at) == 3 {
        panic!("boom at {at}");
    }
}

fn main() {
    let caught = panic::catch_unwind(|| {
        let _a = D("a");
        for i in 0..5 {
            let _loop = D("loop");
            {
                let _b = D("b");
                boom(i);
            }
            println!("iteration {i}");
        }
    });
    println!("caught: {}", caught.is_err());
}
