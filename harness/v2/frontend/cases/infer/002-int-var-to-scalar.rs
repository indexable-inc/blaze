//@ expect: accept
fn main() {
    let x = 200;
    let y: u8 = x;
    let z = 7;
    let w: i64 = z;
    assert!(y == 200 && w == 7);
}
