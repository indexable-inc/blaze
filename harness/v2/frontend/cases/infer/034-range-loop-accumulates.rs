//@ expect: accept
fn main() {
    let mut total = 0;
    for i in 0..4u32 {
        total += i;
    }
    let t: u32 = total;
    assert!(t == 6);
}
