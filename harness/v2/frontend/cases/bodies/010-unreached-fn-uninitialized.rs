//@ expect: reject E0381
fn unused() -> u32 {
    let x: u32;
    x + 1 //~ ERROR E0381
}

fn main() {}
