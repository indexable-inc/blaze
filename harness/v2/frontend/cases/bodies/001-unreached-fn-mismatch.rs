//@ expect: reject E0308
fn unused() -> u32 {
    "not a number" //~ ERROR E0308
}

fn main() {}
