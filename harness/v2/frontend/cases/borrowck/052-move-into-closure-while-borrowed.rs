//@ expect: reject E0505
fn main() {
    let s = String::from("m");
    let r = &s;
    let c = move || s.len(); //~ ERROR E0505
    println!("{} {}", r, c());
}
