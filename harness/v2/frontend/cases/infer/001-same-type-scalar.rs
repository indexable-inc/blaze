//@ expect: accept
fn take(x: u32) -> u32 {
    x + 1
}

fn main() {
    let x: u32 = 5;
    let y: u32 = x;
    assert!(take(y) == 6);
}
