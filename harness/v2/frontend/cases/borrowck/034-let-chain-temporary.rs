//@ expect: reject E0716
fn main() {
    let n = Some(1);
    let r: &str;
    if let Some(_) = n && let Some(s) = String::from("xy").get(0..1) { //~ ERROR E0716
        r = s;
    } else {
        r = "none";
    }
    println!("{r}");
}
