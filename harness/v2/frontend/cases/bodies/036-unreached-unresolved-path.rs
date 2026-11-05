//@ expect: reject E0433
fn unused() -> u32 {
    no_such_module::value() //~ ERROR E0433
}

fn main() {}
