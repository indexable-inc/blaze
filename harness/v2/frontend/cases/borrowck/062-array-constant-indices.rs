//@ expect: accept
fn main() {
    let mut a = [1, 2, 3];
    let [x, y, _] = &mut a;
    *x += 10;
    *y += 20;
    let z = a[0] + a[2];
    println!("{:?} {z}", a);
    let strs = [String::from("p"), String::from("q"), String::from("r")];
    let [p, _, r] = strs;
    println!("{p} {r}");
    let mut b = [0u8; 4];
    let [first, .., last] = &mut b;
    *first = 1;
    *last = 9;
    println!("{:?}", b);
}
