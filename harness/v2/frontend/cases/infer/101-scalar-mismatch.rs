//@ expect: reject E0308
fn main() {
    let x = 5u32;
    let _y: u8 = x; //~ ERROR E0308
}
