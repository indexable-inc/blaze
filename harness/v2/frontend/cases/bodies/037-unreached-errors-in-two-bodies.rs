//@ expect: reject E0308
fn first() -> u32 {
    "one" //~ ERROR E0308
}

fn second() -> bool {
    2 //~ ERROR E0308
}

fn main() {}
