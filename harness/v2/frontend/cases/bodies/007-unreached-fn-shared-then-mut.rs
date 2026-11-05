//@ expect: reject E0502
fn unused() -> i32 {
    let mut v = vec![1, 2, 3];
    let first = &v[0];
    v.push(4); //~ ERROR E0502
    *first
}

fn main() {}
