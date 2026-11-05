//@ expect: reject E0505 E0382
fn main() {
    let s = String::from("a");
    let r = &s;
    let t = s; //~ ERROR E0505
    let n = r.len();
    let u = s; //~ ERROR E0382
    println!("{t} {n} {u}");
}
