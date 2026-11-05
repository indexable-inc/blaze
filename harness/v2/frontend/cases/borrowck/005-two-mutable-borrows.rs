//@ expect: reject E0499
fn main() {
    let mut v = vec![1];
    let a = &mut v;
    let b = &mut v; //~ ERROR E0499
    a.push(2);
    b.push(3);
    println!("{}", v.len());
}
