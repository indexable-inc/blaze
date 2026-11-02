//@ expect: reject E0505
fn take(_: String) {}

fn unused() -> usize {
    let s = String::from("held");
    let r = &s;
    take(s); //~ ERROR E0505
    r.len()
}

fn main() {}
