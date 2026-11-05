//@ expect: reject E0384
fn unused() -> i32 {
    let x = 1;
    x = 2; //~ ERROR E0384
    x
}

fn main() {}
