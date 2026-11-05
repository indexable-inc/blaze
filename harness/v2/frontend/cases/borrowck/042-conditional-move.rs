//@ expect: reject E0382
fn main() {
    let ok = std::env::args().count() > 0;
    let s = String::from("c");
    if ok {
        drop(s);
    }
    let t = s; //~ ERROR E0382
    println!("{t}");
}
