//@ expect: reject E0308
pub fn exported_but_unused() -> char {
    1u32 //~ ERROR E0308
}

fn main() {}
