//@ expect: accept
use std::panic;

struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

impl D {
    fn with(self, other: &D) -> D {
        println!("with {} {}", self.0, other.0);
        self
    }

    fn fail(&self, other: &D) -> usize {
        panic!("fail {} {}", self.0, other.0);
    }
}

fn main() {
    let caught = panic::catch_unwind(|| {
        let _local = D("local");
        D("receiver").with(&D("arg")).fail(&D("last"));
    });
    println!("caught: {}", caught.is_err());
}
