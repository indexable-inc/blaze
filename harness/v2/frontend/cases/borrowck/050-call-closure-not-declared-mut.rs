//@ expect: reject E0596
fn main() {
    let mut x = 0;
    let inc = || x += 1;
    inc(); //~ ERROR E0596
    println!("{x}");
}
