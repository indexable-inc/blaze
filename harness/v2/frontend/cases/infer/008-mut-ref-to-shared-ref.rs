//@ expect: accept
fn read(x: &u32) -> u32 {
    *x
}

fn main() {
    let mut x = 3u32;
    let r = &mut x;
    let s: &u32 = r;
    assert!(read(s) == 3);
    let t = &mut x;
    assert!(read(t) == 3);
}
