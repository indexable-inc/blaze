//@ expect: reject E0505
fn main() {
    let s = String::from("borrowed");
    let r = &s;
    let t = s; //~ ERROR E0505
    println!("{r} {t}");
}
