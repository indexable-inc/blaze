//@ expect: reject E0502
fn main() {
    let mut v = vec![1usize];
    let m = &mut v;
    let n = v.len(); //~ ERROR E0502
    m.push(n);
    println!("{}", v.len());
}
