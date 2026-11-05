//@ expect: reject E0382
fn main() {
    let s = String::from("captured");
    let c = move || s.len();
    let n = s.len(); //~ ERROR E0382
    println!("{} {}", c(), n);
}
