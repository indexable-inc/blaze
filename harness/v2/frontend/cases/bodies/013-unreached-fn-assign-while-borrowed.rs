//@ expect: reject E0506
fn unused() -> i32 {
    let mut x = 1;
    let r = &x;
    x = 2; //~ ERROR E0506
    *r
}

fn main() {}
