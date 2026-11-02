//@ expect: accept
fn main() {
    let x = 11u32;
    let p: *const u32 = &x;
    let mut y = 12u32;
    let q: *mut u32 = &mut y;
    unsafe {
        assert!(*p == 11);
        *q = 13;
    }
    let r: *const u32 = &mut y;
    unsafe {
        assert!(*r == 13);
    }
}
