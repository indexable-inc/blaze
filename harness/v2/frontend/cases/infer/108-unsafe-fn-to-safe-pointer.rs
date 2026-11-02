//@ expect: reject E0308
unsafe fn danger(x: u32) -> u32 {
    x
}

fn main() {
    let _f: fn(u32) -> u32 = danger; //~ ERROR E0308
}
