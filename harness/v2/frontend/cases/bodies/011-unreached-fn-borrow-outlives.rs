//@ expect: reject E0597
fn unused() -> usize {
    let r;
    {
        let s = String::from("short");
        r = &s; //~ ERROR E0597
    }
    r.len()
}

fn main() {}
