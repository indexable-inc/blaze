//@ expect: accept
use std::panic;

struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

struct S {
    a: D,
    b: D,
    c: D,
}

fn boom(name: &'static str) -> D {
    if std::hint::black_box(name) == "b" {
        panic!("field {name}");
    }
    D(name)
}

fn main() {
    let caught = panic::catch_unwind(|| {
        let _before = D("before");
        let s = S { a: boom("a"), b: boom("b"), c: boom("c") };
        println!("built {} {} {}", s.a.0, s.b.0, s.c.0);
    });
    println!("caught: {}", caught.is_err());
    let tuple = panic::catch_unwind(|| {
        let t = (D("t0"), D("t1"), boom("b"));
        println!("{}", t.0 .0);
    });
    println!("caught: {}", tuple.is_err());
}
