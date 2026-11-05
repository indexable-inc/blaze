//@ expect: reject E0597
fn main() {
    let r;
    {
        let x = 5;
        r = &x; //~ ERROR E0597
    }
    println!("{r}");
}
