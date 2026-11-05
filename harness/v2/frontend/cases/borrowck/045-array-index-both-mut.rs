//@ expect: reject E0499
fn main() {
    let mut a = [1, 2, 3];
    let x = &mut a[0];
    let y = &mut a[1]; //~ ERROR E0499
    *x += 1;
    *y += 1;
    println!("{:?}", a);
}
