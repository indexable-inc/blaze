//@ expect: reject E0506
fn main() {
    let mut x = 1;
    let r = &x;
    x = 2; //~ ERROR E0506
    println!("{r} {x}");
}
