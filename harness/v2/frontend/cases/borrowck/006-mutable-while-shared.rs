//@ expect: reject E0502
fn main() {
    let mut v = vec![1];
    let first = &v[0];
    v.push(2); //~ ERROR E0502
    println!("{first}");
}
