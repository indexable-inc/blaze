//@ expect: reject E0596
fn main() {
    let v = Vec::new();
    v.push(1); //~ ERROR E0596
    println!("{}", v.len());
}
