//@ expect: reject E0308
fn main() {
    let s = "abc";
    let _b: &[u8] = s; //~ ERROR E0308
}
