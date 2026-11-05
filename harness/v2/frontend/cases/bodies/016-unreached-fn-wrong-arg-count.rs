//@ expect: reject E0061
fn two(a: u8, b: u8) -> u8 {
    a + b
}

fn unused() -> u8 {
    two(1) //~ ERROR E0061
}

fn main() {
    let _ = two(1, 2);
}
