//@ expect: reject E0499
fn main() {
    let mut v = vec![1, 2];
    v.push(v.pop().unwrap()); //~ ERROR E0499
    println!("{:?}", v);
}
