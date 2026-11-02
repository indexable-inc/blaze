//@ expect: reject E0308
fn main() {
    let n = 3u32;
    let _f: fn(u32) -> u32 = |x| x + n; //~ ERROR E0308
}
