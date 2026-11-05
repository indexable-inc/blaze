//@ expect: reject E0503
fn main() {
    let mut x = 1;
    let r = &mut x;
    let y = x + 1; //~ ERROR E0503
    *r += y;
    println!("{x}");
}
