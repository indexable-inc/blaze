//@ expect: reject E0277
fn unused() -> usize {
    std::mem::size_of::<str>() //~ ERROR E0277
}

fn main() {}
