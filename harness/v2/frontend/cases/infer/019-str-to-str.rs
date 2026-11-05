//@ expect: accept
fn len(s: &str) -> usize {
    s.len()
}

fn main() {
    let s = "hello";
    let t: &str = s;
    assert!(len(t) == 5);
    assert!(len("ab") == 2);
}
