//@ expect: accept
fn read(p: *const u32) -> u32 {
    unsafe { *p }
}

fn main() {
    let mut x = 4u32;
    let p: *mut u32 = &mut x;
    let q: *const u32 = p;
    assert!(read(q) == 4);
    assert!(read(p) == 4);
}
