//@ expect: reject E0308
struct Limits;

impl Limits {
    const MAX: u8 = 1.5; //~ ERROR E0308
}

fn main() {
    let _ = Limits;
}
