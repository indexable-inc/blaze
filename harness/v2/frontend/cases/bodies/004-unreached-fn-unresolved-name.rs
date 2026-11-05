//@ expect: reject E0425
fn unused() -> u32 {
    no_such_value //~ ERROR E0425
}

fn main() {}
