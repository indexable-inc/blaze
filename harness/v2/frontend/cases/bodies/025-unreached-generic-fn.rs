//@ expect: reject E0599
fn never_instantiated<T>(value: T) -> usize {
    value.len() //~ ERROR E0599
}

fn main() {}
