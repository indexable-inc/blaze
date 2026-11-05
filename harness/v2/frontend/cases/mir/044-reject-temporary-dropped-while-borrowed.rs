//@ expect: reject E0716
fn main() {
    let text: &str = String::from("temporary").as_str(); //~ ERROR E0716
    println!("{text}");
}
