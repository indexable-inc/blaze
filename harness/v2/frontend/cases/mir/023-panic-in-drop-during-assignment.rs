//@ expect: accept
use std::panic;

struct P {
    name: &'static str,
    fail: bool,
}

impl Drop for P {
    fn drop(&mut self) {
        println!("drop {}", self.name);
        if self.fail {
            panic!("dropping {}", self.name);
        }
    }
}

fn main() {
    let mut x = P { name: "old", fail: true };
    let caught = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        x = P { name: "new", fail: false };
        println!("assigned");
    }));
    println!("caught: {} holding {}", caught.is_err(), x.name);
}
