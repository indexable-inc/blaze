//@ expect: reject E0597
fn main() {
    let r;
    {
        let s = String::from("inner");
        r = &s; //~ ERROR E0597
    }
    println!("{r}");
}
