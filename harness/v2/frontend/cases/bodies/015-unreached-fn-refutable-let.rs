//@ expect: reject E0005
fn unused(it: Option<u8>) -> u8 {
    let Some(x) = it; //~ ERROR E0005
    x
}

fn main() {}
