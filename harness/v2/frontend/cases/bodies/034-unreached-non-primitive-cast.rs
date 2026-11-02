//@ expect: reject E0605
fn unused() -> Vec<u8> {
    0u8 as Vec<u8> //~ ERROR E0605
}

fn main() {}
