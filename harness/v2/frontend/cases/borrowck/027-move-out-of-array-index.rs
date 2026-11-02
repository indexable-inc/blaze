//@ expect: reject E0508
fn main() {
    let a = [String::from("x"), String::from("y")];
    let s = a[1]; //~ ERROR E0508
    println!("{s}");
}
