//@ expect: reject E0308
fn main() {
    let a = [1u32, 2, 3];
    let _s: &[u8] = &a; //~ ERROR E0308
}
