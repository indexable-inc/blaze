//@ expect: reject E0499
fn unused() {
    let mut v = vec![1, 2, 3];
    let a = &mut v;
    let b = &mut v; //~ ERROR E0499
    a.push(4);
    b.push(5);
}

fn main() {}
