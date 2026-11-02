//@ expect: reject E0384
fn main() {
    let x;
    x = 1;
    println!("{x}");
    x = 2; //~ ERROR E0384
    println!("{x}");
}
