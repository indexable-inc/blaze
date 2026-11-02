//@ expect: reject E0282
fn main() {
    let _x = None; //~ ERROR E0282
}
