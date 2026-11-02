//@ expect: accept
fn total(xs: &[u32]) -> u32 {
    let mut sum = 0;
    let mut i = 0;
    while i < xs.len() {
        sum += xs[i];
        i += 1;
    }
    sum
}

fn main() {
    let a = [1u32, 2, 3];
    let s: &[u32] = &a;
    assert!(total(s) == 6);
    assert!(total(&a) == 6);
    assert!(total(&[4, 5]) == 9);
}
