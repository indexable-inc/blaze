//@ expect: reject E0716
fn main() {
    let r = String::from("temp").as_str(); //~ ERROR E0716
    let n = r.len();
    println!("{n}");
}
