//@ expect: reject E0308
fn nothing() {}

fn main() {
    let _x: u32 = nothing(); //~ ERROR E0308
}
