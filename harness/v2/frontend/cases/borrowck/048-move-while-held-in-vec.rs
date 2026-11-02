//@ expect: reject E0505
fn main() {
    let s = String::from("s");
    let mut v: Vec<&String> = Vec::new();
    v.push(&s);
    drop(s); //~ ERROR E0505
    println!("{}", v.len());
}
