//@ expect: accept
use std::panic;

struct D(String);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

fn d(name: &str) -> D {
    D(name.to_string())
}

fn fails(at: u32, now: u32) -> u32 {
    if at == now {
        panic!("at {at}");
    }
    now
}

/// Every way out of a scope, chosen by `at`: a panic in a guard, an `if` condition, a loop
/// condition, a `match` scrutinee, a `let else` initializer and a closure body.
fn shapes(at: u32) -> u32 {
    let _fn = d("fn");
    let mut total = 0;
    match fails(at, 0) {
        n if fails(at, 1) > n => {
            let _arm = d("arm");
            total += n;
        }
        _ => {}
    }
    if fails(at, 2) == 2 && { let _cond = d("cond"); fails(at, 3) == 3 } {
        let _then = d("then");
        total += 1;
    }
    let mut i = 0;
    while { let _w = d("while cond"); fails(at, 4 + i) < 6 } {
        let _body = d("while body");
        i += 1;
    }
    let Some(v) = Some(fails(at, 6)).filter(|_| { let _f = d("filter"); true }) else {
        return 0;
    };
    let closure = |x: u32| {
        let _c = d("closure");
        fails(at, x)
    };
    total + v + closure(7)
}

fn main() {
    for at in 0..9 {
        let result = panic::catch_unwind(|| shapes(at));
        println!("at {at}: {:?}", result.ok());
    }
}
