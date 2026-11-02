//@ expect: reject E0382
fn reached() -> u32 {
    40 + 2
}

fn unreached() -> usize {
    let s = String::from("x");
    let t = s;
    s.len() + t.len() //~ ERROR E0382
}

fn main() {
    println!("{}", reached());
}
