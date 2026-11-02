//@ expect: accept
fn main() {
    let x = 1;
    let y = x + 2;
    assert!(y == 3);
    let big = 3_000_000_000u64;
    let z = big / 3;
    assert!(z == 1_000_000_000);
}
