//@ expect: accept
fn main() {
    let t: (u32, u8, &[u32]) = (1, 2, &[3, 4]);
    let (a, b, c) = t;
    assert!(a == 1 && b == 2 && c.len() == 2);
    let u = (5, 6);
    let v: (u64, i8) = u;
    assert!(v.0 == 5 && v.1 == 6);
}
