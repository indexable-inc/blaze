//@ expect: accept
fn bump(xs: &mut [u32]) {
    let mut i = 0;
    while i < xs.len() {
        xs[i] += 1;
        i += 1;
    }
}

fn main() {
    let mut a = [1u32, 2];
    bump(&mut a);
    let s: &mut [u32] = &mut a;
    bump(s);
    assert!(a[0] == 3 && a[1] == 4);
}
