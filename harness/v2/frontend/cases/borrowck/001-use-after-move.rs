//@ expect: reject E0382
fn consume(s: String) -> usize {
    s.len()
}

fn main() {
    let s = String::from("moved");
    let n = consume(s);
    let m = consume(s); //~ ERROR E0382
    println!("{n} {m}");
}
