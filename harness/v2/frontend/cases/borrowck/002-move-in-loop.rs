//@ expect: reject E0382
fn main() {
    let s = String::from("loop");
    let mut total = 0;
    for _ in 0..2 {
        let t = s; //~ ERROR E0382
        total += t.len();
    }
    println!("{total}");
}
