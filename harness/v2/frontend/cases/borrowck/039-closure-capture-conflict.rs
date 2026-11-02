//@ expect: reject E0502
fn main() {
    let mut x = 0;
    let mut inc = || x += 1;
    let r = &x; //~ ERROR E0502
    inc();
    println!("{r}");
}
