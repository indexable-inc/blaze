//@ expect: reject E0308
fn main() {
    let n = 2u32;
    let _x = match n {
        0 => 0u8,
        1 => 1u8,
        _ => 2u16, //~ ERROR E0308
    };
}
