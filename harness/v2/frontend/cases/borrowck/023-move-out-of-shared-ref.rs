//@ expect: reject E0507
fn main() {
    let owned = String::from("r");
    let s = &owned;
    let t = *s; //~ ERROR E0507
    println!("{t}");
}
