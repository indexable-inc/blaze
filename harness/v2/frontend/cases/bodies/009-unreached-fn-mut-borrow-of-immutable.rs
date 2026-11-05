//@ expect: reject E0596
fn unused() {
    let v = Vec::<u8>::new();
    let r = &mut v; //~ ERROR E0596
    r.push(1);
}

fn main() {}
