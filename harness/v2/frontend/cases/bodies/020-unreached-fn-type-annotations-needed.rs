//@ expect: reject E0282
fn unused() {
    let v = Vec::new(); //~ ERROR E0282
    let _ = v.len();
}

fn main() {}
