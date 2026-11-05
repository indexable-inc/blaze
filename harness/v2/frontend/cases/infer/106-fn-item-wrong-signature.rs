//@ expect: reject E0308
fn double(x: u32) -> u32 {
    x * 2
}

fn main() {
    let _f: fn(u8) -> u32 = double; //~ ERROR E0308
}
