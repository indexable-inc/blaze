//@ expect: accept
fn bump(x: &mut u32) {
    *x += 1;
}

fn main() {
    let mut n = 0u32;
    let r = &mut n;
    bump(r);
    bump(r);
    bump(r);
    assert!(n == 3);
}
