//@ expect: reject E0599
fn unused() {
    1u8.no_such_method(); //~ ERROR E0599
}

fn main() {}
