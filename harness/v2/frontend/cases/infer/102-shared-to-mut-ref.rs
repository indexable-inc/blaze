//@ expect: reject E0308
fn bump(x: &mut u32) {
    *x += 1;
}

fn main() {
    let x = 1u32;
    bump(&x); //~ ERROR E0308
}
