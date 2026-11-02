//@ expect: reject E0308
fn main() {
    let x = 1u32;
    let p: *const u32 = &x;
    let _q: *mut u32 = p; //~ ERROR E0308
}
