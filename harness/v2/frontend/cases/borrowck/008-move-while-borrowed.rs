//@ expect: reject E0505
fn consume(s: String) -> usize {
    s.len()
}

fn main() {
    let s = String::from("borrowed");
    let r = &s;
    let n = consume(s); //~ ERROR E0505
    println!("{r} {n}");
}
