//@ expect: reject E0499 E0502
fn main() {
    let mut a = vec![1];
    let r1 = &mut a;
    let r2 = &mut a; //~ ERROR E0499
    r1.push(1);
    r2.push(2);
    let mut b = vec![2];
    let s1 = &b;
    b.push(3); //~ ERROR E0502
    println!("{}", s1.len());
}
