//@ expect: reject E0382
fn consume(s: String) -> usize {
    s.len()
}

fn unused() -> usize {
    let s = String::from("moved");
    let a = consume(s);
    a + consume(s) //~ ERROR E0382
}

fn main() {}
