//@ expect: accept
fn double(x: u32) -> u32 {
    x * 2
}

fn apply(f: fn(u32) -> u32, x: u32) -> u32 {
    f(x)
}

fn main() {
    let f: fn(u32) -> u32 = double;
    assert!(apply(f, 3) == 6);
    assert!(apply(double, 4) == 8);
}
