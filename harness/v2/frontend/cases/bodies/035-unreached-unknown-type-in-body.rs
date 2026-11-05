//@ expect: reject E0412
fn unused() {
    let _x: NoSuchType = (); //~ ERROR E0412
}

fn main() {}
