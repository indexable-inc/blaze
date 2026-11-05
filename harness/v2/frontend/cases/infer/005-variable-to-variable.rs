//@ expect: accept
fn main() {
    let a = None;
    let b = a;
    let c: Option<u32> = b;
    let d = c;
    assert!(d.is_none());
}
