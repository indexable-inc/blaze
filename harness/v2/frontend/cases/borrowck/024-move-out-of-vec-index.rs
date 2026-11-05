//@ expect: reject E0507
fn main() {
    let v = vec![String::from("a"), String::from("b")];
    let s = v[0]; //~ ERROR E0507
    println!("{s}");
}
