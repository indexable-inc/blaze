//@ expect: accept
fn pick(flag: bool) -> usize {
    let s: &[u32] = if flag { &[1, 2] } else { &[3] };
    s.len()
}

fn main() {
    assert!(pick(true) == 2);
    assert!(pick(false) == 1);
}
